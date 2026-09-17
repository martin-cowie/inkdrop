use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode as HttpStatusCode;
use ipp::attribute::{IppAttribute, IppAttributes};
use ipp::model::{DelimiterTag, JobState, Operation, PrinterState, StatusCode};
use ipp::parser::IppParser;
use ipp::request::IppRequestResponse;
use ipp::value::IppValue;
use num_traits::FromPrimitive;
use tracing::{info, warn};

use crate::jobs::JobStore;

#[derive(Clone)]
pub struct AppState {
    pub jobs: JobStore,
    pub next_job_id: Arc<AtomicI32>,
}

/// Handles a raw `application/ipp` POST body and returns the raw response
/// bytes (header + attributes; no trailing payload — this simulator never
/// sends a document back).
pub async fn handle(State(state): State<AppState>, body: Bytes) -> (HttpStatusCode, Bytes) {
    let response = match process(&state, body) {
        Ok(response) => response,
        Err(err) => {
            warn!(%err, "failed to parse IPP request");
            error_response(StatusCode::ClientErrorBadRequest, 0)
        }
    };
    (HttpStatusCode::OK, response.to_bytes())
}

fn process(state: &AppState, body: Bytes) -> Result<IppRequestResponse, ipp::parser::IppParseError> {
    let cursor = Cursor::new(body);
    let (header, attributes, reader) = IppParser::new(cursor).parse_parts()?;

    let mut document = Vec::new();
    reader.into_inner().read_to_end(&mut document).expect("reading from an in-memory buffer cannot fail");

    let operation = Operation::from_i16(header.operation_or_status);
    info!(?operation, request_id = header.request_id, document_bytes = document.len(), "received IPP request");

    let response = match operation {
        Some(Operation::GetPrinterAttributes) => get_printer_attributes(header.request_id),
        Some(Operation::PrintJob) => print_job(state, &attributes, document, header.request_id),
        Some(Operation::ValidateJob) => success_response(header.request_id),
        _ => {
            warn!(?operation, "unhandled IPP operation; returning a generic success");
            success_response(header.request_id)
        }
    };

    Ok(response)
}

fn success_response(request_id: i32) -> IppRequestResponse {
    IppRequestResponse::new_response(ipp::model::IppVersion::v1_1(), StatusCode::SuccessfulOk, request_id)
        .expect("infallible: only ASCII in the fixed charset/language values")
}

fn error_response(status: StatusCode, request_id: i32) -> IppRequestResponse {
    IppRequestResponse::new_response(ipp::model::IppVersion::v1_1(), status, request_id)
        .expect("infallible: only ASCII in the fixed charset/language values")
}

fn attr(name: &str, value: IppValue) -> IppAttribute {
    IppAttribute::with_name(name, value).expect("attribute name is a valid static string")
}

fn get_printer_attributes(request_id: i32) -> IppRequestResponse {
    let mut response = success_response(request_id);
    let attrs: &mut IppAttributes = response.attributes_mut();

    let group = DelimiterTag::PrinterAttributes;
    attrs.add(group, attr("printer-name", IppValue::new_text_without_language("Inkdrop Simulated Printer").unwrap()));
    attrs.add(group, attr("printer-state", IppValue::new_enum(PrinterState::Idle).unwrap()));
    attrs.add(group, attr("printer-state-reasons", IppValue::new_keyword("none").unwrap()));
    attrs.add(group, attr("printer-is-accepting-jobs", IppValue::new_boolean(true)));
    attrs.add(
        group,
        attr(
            "document-format-supported",
            IppValue::Array(vec![
                IppValue::new_mime_media_type("image/pwg-raster").unwrap(),
                IppValue::new_mime_media_type("image/urf").unwrap(),
            ]),
        ),
    );
    attrs.add(
        group,
        attr("document-format-default", IppValue::new_mime_media_type("image/pwg-raster").unwrap()),
    );
    attrs.add(group, attr("pwg-raster-document-resolution-supported", IppValue::new_resolution(300, 300, 3)));
    attrs.add(
        group,
        attr(
            "pwg-raster-document-type-supported",
            IppValue::Array(vec![IppValue::new_keyword("srgb_8").unwrap(), IppValue::new_keyword("sgray_8").unwrap()]),
        ),
    );

    response
}

fn print_job(state: &AppState, request: &IppAttributes, document: Vec<u8>, request_id: i32) -> IppRequestResponse {
    let op_group = request.first_of(DelimiterTag::OperationAttributes);
    let document_format = op_group
        .and_then(|g| g.get("document-format"))
        .map(|a| a.value().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let job_name = op_group.and_then(|g| g.get("job-name")).map(|a| a.value().to_string());

    let job_id = state.next_job_id.fetch_add(1, Ordering::SeqCst);

    match state.jobs.save(job_id, &document_format, job_name.as_deref(), &document) {
        Ok(saved) => {
            info!(job_id, %document_format, bytes = document.len(), path = %saved.path.display(), pages_previewed = saved.preview_pages, "saved print job");
            if let Some(preview_error) = saved.preview_error {
                warn!(job_id, %document_format, error = %preview_error, "document claimed to be PWG-Raster but failed to decode — treating as a real printer would: rejecting the job");
                let mut response = error_response(StatusCode::ClientErrorDocumentFormatError, request_id);
                response
                    .attributes_mut()
                    .add(DelimiterTag::JobAttributes, attr("job-id", IppValue::new_integer(job_id)));
                return response;
            }
        }
        Err(err) => {
            warn!(job_id, %document_format, %err, "failed to save print job");
        }
    }

    let mut response = success_response(request_id);
    let attrs = response.attributes_mut();
    let group = DelimiterTag::JobAttributes;
    attrs.add(group, attr("job-id", IppValue::new_integer(job_id)));
    attrs.add(group, attr("job-state", IppValue::new_enum(JobState::Completed).unwrap()));
    attrs.add(group, attr("job-state-reasons", IppValue::new_keyword("job-completed-successfully").unwrap()));

    response
}
