use std::io::Cursor;
use std::net::IpAddr;

use bytes::Bytes;
use ipp::parser::IppParseError;
use ipp::prelude::*;
use tracing::{debug, info, info_span, trace, warn, Instrument};

use crate::discovery::Printer;
use crate::raster::ColorMode;

#[derive(Debug, thiserror::Error)]
pub enum PrintError {
    #[error("printer does not support PDF, directly or via raster conversion")]
    UnsupportedFormat,
    #[error("invalid printer URI: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("IPP request failed: {0}")]
    Ipp(#[from] IppError),
    #[error("IPP build error: {0}")]
    IppBuild(#[from] IppParseError),
    #[error("PDF rendering failed: {0}")]
    Render(#[from] crate::pdf::RenderError),
    #[error("printer rejected the request: {status:?}{}", status_detail(.message))]
    Rejected { status: StatusCode, message: Option<String> },
}

fn status_detail(message: &Option<String>) -> String {
    message.as_deref().map(|m| format!(" ({m})")).unwrap_or_default()
}

/// A document format inkdrop can send to a printer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentFormat {
    Pdf,
    PwgRaster,
}

impl DocumentFormat {
    pub fn mime_type(self) -> &'static str {
        match self {
            DocumentFormat::Pdf => "application/pdf",
            DocumentFormat::PwgRaster => "image/pwg-raster",
        }
    }
}

/// How a PDF will actually reach the printer.
#[derive(Debug)]
pub enum PrintPlan {
    /// The printer accepts `application/pdf` directly; send the bytes as-is.
    DirectPdf,
    /// The printer doesn't take PDF, but does take `image/pwg-raster`; the
    /// PDF is rasterized locally first.
    PwgRaster { dpi: u32, color: ColorMode },
}

/// What the printer said in response to a successful Print-Job.
#[derive(Debug)]
pub struct Submitted {
    pub status: StatusCode,
    pub job_id: Option<i32>,
    pub job_state: Option<String>,
    pub job_state_reasons: Vec<String>,
}

/// Confirm how the printer wants the document, converting if necessary, then
/// submit it as a Print-Job.
pub async fn print_pdf(printer: &Printer, job_title: &str, client_ip: IpAddr, document: Bytes) -> Result<(), PrintError> {
    let span = info_span!("print", %client_ip, %job_title);
    async {
        let uri = printer.uri.clone();
        let client = AsyncIppClient::new(uri.clone());

        let plan = plan_print(&client, uri.clone(), None).await?;
        let (payload, format) = render(&plan, document)?;
        submit(&client, uri, payload, format, job_title).await?;

        info!("print job accepted by printer");
        Ok(())
    }
    .instrument(span)
    .await
}

/// Convert `document` into the form `plan` calls for, returning the bytes to
/// send and their format.
pub fn render(plan: &PrintPlan, document: Bytes) -> Result<(Bytes, DocumentFormat), PrintError> {
    match *plan {
        PrintPlan::DirectPdf => {
            info!("printer accepts PDF directly; sending document as-is");
            Ok((document, DocumentFormat::Pdf))
        }
        PrintPlan::PwgRaster { dpi, color } => {
            info!(dpi, ?color, "printer requires PWG-Raster; rendering PDF page by page");
            let pages = crate::pdf::render_pages(&document, dpi as f32)?;
            let raster = crate::raster::encode(&pages, dpi, color);
            info!(page_count = pages.len(), bytes = raster.len(), "encoded raster document");
            Ok((raster, DocumentFormat::PwgRaster))
        }
    }
}

/// Send `payload` to the printer as a single Print-Job request.
pub async fn submit(
    client: &AsyncIppClient,
    uri: Uri,
    payload: Bytes,
    format: DocumentFormat,
    job_title: &str,
) -> Result<Submitted, PrintError> {
    let operation = IppOperationBuilder::print_job(uri, IppPayload::new(Cursor::new(payload)))
        .user_name("inkdrop")
        .job_title(job_title)
        .document_format(format.mime_type())
        .build()?;

    let request: IppRequestResponse = operation.into();
    log_attributes("Print-Job request", request.attributes());

    let response = client.send(request).await?;
    let status = response.header().status_code();
    log_attributes("Print-Job response", response.attributes());

    if !status.is_success() {
        return Err(PrintError::Rejected { status, message: status_message(&response) });
    }

    let job = response.attributes().first_of(DelimiterTag::JobAttributes);
    let job_attr = |name: &str| job.and_then(|g| g.get(name)).map(|attr| attr.value());

    Ok(Submitted {
        status,
        job_id: job_attr("job-id").and_then(|v| v.as_integer().copied()),
        job_state: job_attr("job-state").map(|v| match v {
            IppValue::Enum(n) => JobState::try_from(*n).map(|s| format!("{s:?}")).unwrap_or_else(|_| n.to_string()),
            other => other.to_string(),
        }),
        job_state_reasons: job_attr("job-state-reasons")
            .map(|v| flatten(v).iter().map(|r| r.to_string()).collect())
            .unwrap_or_default(),
    })
}

/// What a printer says about itself when asked directly over IPP.
#[derive(Debug)]
pub struct PrinterInfo {
    /// `printer-info`, falling back to `printer-name`.
    pub name: Option<String>,
    pub model: Option<String>,
    /// Display labels of the formats it accepts (see
    /// [`crate::raster::matching_labels`]).
    pub formats: Vec<&'static str>,
}

/// Query the printer directly for the formats it advertises, for use
/// when mDNS TXT records don't mention "pdl" at all. Any failure
/// (unreachable, malformed response, etc.) is treated as "none" rather than
/// propagated, since this is a best-effort discovery-time probe.
pub async fn probe_formats(uri: &Uri) -> Vec<&'static str> {
    match probe_printer(uri).await {
        Ok(info) => info.formats,
        Err(err) => {
            tracing::debug!(%uri, error = %err, "failed to probe printer capabilities");
            Vec::new()
        }
    }
}

/// Ask the printer for its name, model and supported document formats.
pub async fn probe_printer(uri: &Uri) -> Result<PrinterInfo, PrintError> {
    let client = AsyncIppClient::new(uri.clone());
    let operation = IppOperationBuilder::get_printer_attributes(uri.clone())
        .attribute(IppAttribute::DOCUMENT_FORMAT_SUPPORTED)
        .attribute(IppAttribute::PRINTER_NAME)
        .attribute("printer-info")
        .attribute("printer-make-and-model")
        .build()?;

    let response = client.send(operation).await?;
    let status = response.header().status_code();
    if !status.is_success() {
        return Err(PrintError::Rejected { status, message: status_message(&response) });
    }

    let group = response.attributes().first_of(DelimiterTag::PrinterAttributes);
    let text = |name: &str| {
        group
            .and_then(|g| g.get(name))
            .map(|attr| attr.value().to_string())
            .filter(|v| !v.is_empty())
    };
    let formats: Vec<String> = group
        .and_then(|g| g.get(IppAttribute::DOCUMENT_FORMAT_SUPPORTED))
        .map(|attr| flatten(attr.value()).iter().map(|v| v.to_string()).collect())
        .unwrap_or_default();

    Ok(PrinterInfo {
        name: text("printer-info").or_else(|| text(IppAttribute::PRINTER_NAME)),
        model: text("printer-make-and-model"),
        formats: crate::raster::matching_labels(formats.iter().map(String::as_str)),
    })
}

/// Ask the printer what it accepts and decide how to send it a PDF. With
/// `forced`, that format is used even if the printer doesn't advertise it,
/// though raster resolution and color mode still come from the printer.
pub async fn plan_print(client: &AsyncIppClient, uri: Uri, forced: Option<DocumentFormat>) -> Result<PrintPlan, PrintError> {
    let operation = IppOperationBuilder::get_printer_attributes(uri)
        .attribute(IppAttribute::DOCUMENT_FORMAT_SUPPORTED)
        .attribute("pwg-raster-document-resolution-supported")
        .attribute("pwg-raster-document-type-supported")
        .build()?;

    let response = client.send(operation).await?;
    let status = response.header().status_code();
    log_attributes("Get-Printer-Attributes response", response.attributes());

    if !status.is_success() {
        return Err(PrintError::Rejected { status, message: status_message(&response) });
    }

    let group = response.attributes().first_of(DelimiterTag::PrinterAttributes);

    let format_supported = |format: DocumentFormat| {
        group
            .and_then(|g| g.get(IppAttribute::DOCUMENT_FORMAT_SUPPORTED))
            .is_some_and(|attr| flatten(attr.value()).iter().any(|v| value_is(v, format.mime_type())))
    };

    let format = match forced {
        Some(format) => {
            if !format_supported(format) {
                warn!(format = format.mime_type(), "printer does not advertise the forced document format");
            }
            format
        }
        None if format_supported(DocumentFormat::Pdf) => DocumentFormat::Pdf,
        None if format_supported(DocumentFormat::PwgRaster) => DocumentFormat::PwgRaster,
        None => return Err(PrintError::UnsupportedFormat),
    };

    if format == DocumentFormat::Pdf {
        return Ok(PrintPlan::DirectPdf);
    }

    let dpi = group
        .and_then(|g| g.get("pwg-raster-document-resolution-supported"))
        .and_then(|attr| flatten(attr.value()).into_iter().find_map(as_resolution))
        .unwrap_or(300);

    let color = group
        .and_then(|g| g.get("pwg-raster-document-type-supported"))
        .map(|attr| flatten(attr.value()))
        .and_then(|values| pick_color_mode(&values))
        .unwrap_or(ColorMode::Sgray8);

    Ok(PrintPlan::PwgRaster { dpi, color })
}

fn status_message(response: &IppRequestResponse) -> Option<String> {
    response
        .attributes()
        .first_of(DelimiterTag::OperationAttributes)
        .and_then(|g| g.get("status-message"))
        .map(|attr| attr.value().to_string())
}

/// Log every attribute of an IPP message: operation attributes at debug
/// level, everything else (often hundreds of printer attributes) at trace.
fn log_attributes(context: &str, attributes: &IppAttributes) {
    for group in attributes.groups() {
        let tag = group.tag();
        for attr in group.attributes() {
            if tag == DelimiterTag::OperationAttributes || tag == DelimiterTag::JobAttributes {
                debug!(?tag, name = %attr.name(), value = %attr.value(), "{context}");
            } else {
                trace!(?tag, name = %attr.name(), value = %attr.value(), "{context}");
            }
        }
    }
}

/// Attribute values are either a single `IppValue` or an `Array` of them;
/// present both uniformly as a list.
fn flatten(value: &IppValue) -> Vec<&IppValue> {
    match value {
        IppValue::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

fn value_is(value: &IppValue, expected: &str) -> bool {
    value.to_string().eq_ignore_ascii_case(expected)
}

fn as_resolution(value: &IppValue) -> Option<u32> {
    match value {
        IppValue::Resolution { cross_feed, .. } => Some((*cross_feed).max(1) as u32),
        _ => None,
    }
}

/// Prefer color (srgb_8) over grayscale (sgray_8) when both are offered,
/// regardless of which order the printer listed them in.
fn pick_color_mode(values: &[&IppValue]) -> Option<ColorMode> {
    let has = |name: &str| values.iter().any(|v| value_is(v, name));
    if has("srgb_8") {
        Some(ColorMode::Srgb8)
    } else if has("sgray_8") {
        Some(ColorMode::Sgray8)
    } else {
        None
    }
}
