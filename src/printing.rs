//! Sends a PDF to an IPP printer: asks what it accepts, converts the PDF to
//! PWG-Raster if need be, and submits a Print-Job.

use std::io::Cursor;
use std::net::IpAddr;

use bytes::Bytes;
use ipp::parser::IppParseError;
use ipp::prelude::*;
use tracing::{debug, info, info_span, trace, warn, Instrument};

use crate::discovery::Printer;
use crate::raster::ColorMode;

/// Why a document couldn't be printed.
#[derive(Debug, thiserror::Error)]
pub enum PrintError {
    /// The printer accepts neither PDF nor PWG-Raster.
    #[error("printer does not support PDF, directly or via raster conversion")]
    UnsupportedFormat,
    /// The printer URI couldn't be parsed.
    #[error("invalid printer URI: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),
    /// The printer couldn't be reached, or its response couldn't be read.
    #[error("IPP request failed: {0}")]
    Ipp(#[from] IppError),
    /// The IPP request couldn't be built.
    #[error("IPP build error: {0}")]
    IppBuild(#[from] IppParseError),
    /// The PDF couldn't be rendered for raster conversion.
    #[error("PDF rendering failed: {0}")]
    Render(#[from] crate::pdf::RenderError),
    /// The printer answered with an unsuccessful IPP status, and its
    /// `status-message` if it gave one.
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
    /// The IPP `document-format` value for this format.
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
    PwgRaster {
        /// Resolution to render at, in dots per inch.
        dpi: u32,
        /// Colour mode to encode in.
        color: ColorMode,
    },
}

/// What the printer said in response to a successful Print-Job.
#[derive(Debug)]
pub struct Submitted {
    /// The IPP status of the response.
    pub status: StatusCode,
    /// `job-id`, if the printer gave one.
    pub job_id: Option<i32>,
    /// `job-state`, e.g. "Pending", if the printer gave one.
    pub job_state: Option<String>,
    /// `job-state-reasons`, e.g. "none"; empty if the printer gave none.
    pub job_state_reasons: Vec<String>,
}

/// Print the PDF `document` on `printer` as a job titled `job_title`: ask the
/// printer how it wants the document, convert it if necessary, and submit it
/// as a Print-Job. Logging is tagged with `client_ip`, the uploader's address.
///
/// # Errors
///
/// [`PrintError::UnsupportedFormat`] if the printer takes neither PDF nor
/// PWG-Raster, and any error from [`plan_print`], [`render`] or [`submit`].
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

/// Convert the PDF `document` into the form `plan` calls for, returning the
/// bytes to send and their format.
///
/// # Errors
///
/// [`PrintError::Render`] if the PDF can't be rasterised.
///
/// # Panics
///
/// If rasterising is needed and PDFium can't be loaded; see
/// [`crate::pdf::ensure_available`].
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

/// Send `payload`, a document in `format`, to the printer at `uri` via
/// `client` as a single Print-Job titled `job_title`.
///
/// Returns what the printer said about the new job.
///
/// # Errors
///
/// [`PrintError::Rejected`] if the printer refuses the job,
/// [`PrintError::Ipp`] if it can't be reached, and [`PrintError::IppBuild`]
/// if the request can't be built.
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
    /// `printer-make-and-model`.
    pub model: Option<String>,
    /// Display labels of the formats it accepts (see
    /// [`crate::raster::matching_labels`]).
    pub formats: Vec<&'static str>,
}

/// Ask the printer at `uri` which supported formats it accepts, for use when
/// its mDNS TXT record has no "pdl" key. Returns their display labels. Any
/// failure (unreachable, malformed response, etc.) is treated as "none"
/// rather than propagated, since this is a best-effort discovery-time probe.
pub async fn probe_formats(uri: &Uri) -> Vec<&'static str> {
    match probe_printer(uri).await {
        Ok(info) => info.formats,
        Err(err) => {
            tracing::debug!(%uri, error = %err, "failed to probe printer capabilities");
            Vec::new()
        }
    }
}

/// Ask the printer at `uri` for its name, model and supported document
/// formats.
///
/// # Errors
///
/// [`PrintError::Rejected`] if the printer answers with an unsuccessful
/// status, and [`PrintError::Ipp`] if it can't be reached.
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

/// Ask the printer at `uri`, via `client`, what it accepts and decide how to
/// send it a PDF, preferring PDF to PWG-Raster. With `forced`, that format is
/// used even if the printer doesn't advertise it, though raster resolution
/// and color mode still come from the printer.
///
/// # Errors
///
/// [`PrintError::UnsupportedFormat`] if nothing is forced and the printer
/// takes neither format, [`PrintError::Rejected`] if it answers with an
/// unsuccessful status, and [`PrintError::Ipp`] if it can't be reached.
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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use ipp::value::{BoundedString, IppTextValue};

    use super::*;
    use crate::test_support::{self, A4, Config, FakePrinter};

    const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn keyword(value: &str) -> IppValue {
        IppValue::Keyword(BoundedString::new(value).unwrap())
    }

    fn printer_at(fake: &FakePrinter, formats: Vec<&'static str>) -> Printer {
        Printer {
            id: "p1".to_owned(),
            name: "Fake".to_owned(),
            uri: fake.uri().parse().unwrap(),
            model: None,
            formats,
        }
    }

    async fn plan_for(config: Config, forced: Option<DocumentFormat>) -> Result<PrintPlan, PrintError> {
        test_support::init_tracing();
        let fake = FakePrinter::start(config).await;
        let uri: Uri = fake.uri().parse().unwrap();
        plan_print(&AsyncIppClient::new(uri.clone()), uri, forced).await
    }

    #[test]
    fn document_formats_have_mime_types() {
        assert_eq!(DocumentFormat::Pdf.mime_type(), "application/pdf");
        assert_eq!(DocumentFormat::PwgRaster.mime_type(), "image/pwg-raster");
    }

    #[test]
    fn rejection_messages_include_the_printer_status_message() {
        let with = PrintError::Rejected { status: StatusCode::ClientErrorNotPossible, message: Some("jammed".to_owned()) };
        assert_eq!(with.to_string(), "printer rejected the request: ClientErrorNotPossible (jammed)");
        let without = PrintError::Rejected { status: StatusCode::ServerErrorBusy, message: None };
        assert_eq!(without.to_string(), "printer rejected the request: ServerErrorBusy");
    }

    #[test]
    fn values_flatten_and_compare_ignoring_case() {
        let single = keyword("srgb_8");
        assert_eq!(flatten(&single), [&single]);
        let array = IppValue::Array(vec![keyword("a"), keyword("b")]);
        assert_eq!(flatten(&array).len(), 2);

        assert!(value_is(&keyword("Application/PDF"), "application/pdf"));
        assert!(!value_is(&keyword("image/urf"), "application/pdf"));
    }

    #[test]
    fn resolutions_use_the_cross_feed_dpi() {
        assert_eq!(as_resolution(&IppValue::Resolution { cross_feed: 600, feed: 300, units: 3 }), Some(600));
        assert_eq!(as_resolution(&IppValue::Resolution { cross_feed: 0, feed: 0, units: 3 }), Some(1));
        assert_eq!(as_resolution(&IppValue::Integer(300)), None);
    }

    #[test]
    fn color_is_preferred_over_gray() {
        let (gray, color, other) = (keyword("sgray_8"), keyword("srgb_8"), keyword("black_1"));
        assert!(matches!(pick_color_mode(&[&gray, &color]), Some(ColorMode::Srgb8)));
        assert!(matches!(pick_color_mode(&[&gray, &other]), Some(ColorMode::Sgray8)));
        assert!(pick_color_mode(&[&other]).is_none());
    }

    #[tokio::test]
    async fn plans_to_send_pdf_when_the_printer_takes_it() {
        let config = Config { formats: vec!["image/pwg-raster", "application/pdf"], ..Config::default() };
        assert!(matches!(plan_for(config, None).await, Ok(PrintPlan::DirectPdf)));
    }

    #[tokio::test]
    async fn plans_raster_at_the_printers_resolution_and_color() {
        let plan = plan_for(Config::raster_only(600, &["sgray_8", "srgb_8"]), None).await;
        assert!(matches!(plan, Ok(PrintPlan::PwgRaster { dpi: 600, color: ColorMode::Srgb8 })), "{plan:?}");
    }

    #[tokio::test]
    async fn plans_raster_with_defaults_when_the_printer_omits_details() {
        let config = Config { formats: vec!["image/pwg-raster"], ..Config::default() };
        let plan = plan_for(config, None).await;
        assert!(matches!(plan, Ok(PrintPlan::PwgRaster { dpi: 300, color: ColorMode::Sgray8 })), "{plan:?}");
    }

    #[tokio::test]
    async fn refuses_printers_without_pdf_or_pwg_raster() {
        let config = Config { formats: vec!["image/urf"], ..Config::default() };
        assert!(matches!(plan_for(config, None).await, Err(PrintError::UnsupportedFormat)));
    }

    #[tokio::test]
    async fn forced_formats_are_used_even_if_not_advertised() {
        let raster = plan_for(Config::default(), Some(DocumentFormat::PwgRaster)).await;
        assert!(matches!(raster, Ok(PrintPlan::PwgRaster { dpi: 300, .. })), "{raster:?}");

        let pdf = plan_for(Config::raster_only(300, &["srgb_8"]), Some(DocumentFormat::Pdf)).await;
        assert!(matches!(pdf, Ok(PrintPlan::DirectPdf)));

        let advertised = plan_for(Config::default(), Some(DocumentFormat::Pdf)).await;
        assert!(matches!(advertised, Ok(PrintPlan::DirectPdf)));
    }

    #[tokio::test]
    async fn planning_reports_rejections_and_failures() {
        let config = Config {
            attributes_status: StatusCode::ServerErrorBusy,
            status_message: Some("warming up"),
            ..Config::default()
        };
        match plan_for(config, None).await {
            Err(PrintError::Rejected { status, message }) => {
                assert_eq!(status, StatusCode::ServerErrorBusy);
                assert_eq!(message.as_deref(), Some("warming up"));
            }
            other => panic!("expected a rejection, got {other:?}"),
        }

        let offline = Config { online: false, ..Config::default() };
        assert!(matches!(plan_for(offline, None).await, Err(PrintError::Ipp(_))));
    }

    #[tokio::test]
    async fn submit_sends_the_document_and_reports_the_job() {
        test_support::init_tracing();
        let fake = FakePrinter::start(Config::default()).await;
        let uri: Uri = fake.uri().parse().unwrap();
        let client = AsyncIppClient::new(uri.clone());

        let submitted = submit(&client, uri, Bytes::from_static(b"%PDF"), DocumentFormat::Pdf, "report.pdf").await.unwrap();

        assert_eq!(submitted.status, StatusCode::SuccessfulOk);
        assert_eq!(submitted.job_id, Some(42));
        assert_eq!(submitted.job_state.as_deref(), Some("Pending"));
        assert_eq!(submitted.job_state_reasons, ["none"]);

        let jobs = fake.print_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].document, b"%PDF");
        assert_eq!(jobs[0].attr("document-format").as_deref(), Some("application/pdf"));
        assert_eq!(jobs[0].attr("job-name").as_deref(), Some("report.pdf"));
        assert_eq!(jobs[0].attr("requesting-user-name").as_deref(), Some("inkdrop"));
    }

    #[tokio::test]
    async fn submit_copes_with_unusual_job_attributes() {
        let fake = FakePrinter::start(Config::default()).await;
        let uri: Uri = fake.uri().parse().unwrap();
        let client = AsyncIppClient::new(uri.clone());
        let send = || submit(&client, uri.clone(), Bytes::new(), DocumentFormat::PwgRaster, "t");

        fake.configure(|c| {
            c.job_state = Some(IppValue::Enum(99));
            c.job_state_reasons = vec!["job-incoming", "job-queued"];
        });
        let unknown = send().await.unwrap();
        assert_eq!(unknown.job_state.as_deref(), Some("99"));
        assert_eq!(unknown.job_state_reasons, ["job-incoming", "job-queued"]);

        fake.configure(|c| c.job_state = Some(keyword("processing")));
        assert_eq!(send().await.unwrap().job_state.as_deref(), Some("processing"));

        fake.configure(|c| {
            c.job_id = None;
            c.job_state = None;
            c.job_state_reasons = Vec::new();
        });
        let bare = send().await.unwrap();
        assert_eq!((bare.job_id, bare.job_state), (None, None));
        assert!(bare.job_state_reasons.is_empty());
    }

    #[tokio::test]
    async fn submit_reports_a_rejected_job() {
        let fake = FakePrinter::start(Config {
            print_status: StatusCode::ClientErrorDocumentFormatNotSupported,
            ..Config::default()
        })
        .await;
        let uri: Uri = fake.uri().parse().unwrap();
        let result = submit(&AsyncIppClient::new(uri.clone()), uri, Bytes::new(), DocumentFormat::Pdf, "t").await;
        assert!(matches!(
            result,
            Err(PrintError::Rejected { status: StatusCode::ClientErrorDocumentFormatNotSupported, message: None })
        ));
    }

    #[test]
    fn render_passes_pdf_through_untouched() {
        let document = Bytes::from_static(b"anything");
        let (payload, format) = render(&PrintPlan::DirectPdf, document.clone()).unwrap();
        assert_eq!((payload, format), (document, DocumentFormat::Pdf));
    }

    #[test]
    fn render_rasterizes_for_pwg_raster() {
        let pdf = Bytes::from(test_support::pdf(&[A4, A4]));
        let plan = PrintPlan::PwgRaster { dpi: 36, color: ColorMode::Sgray8 };

        let (payload, format) = render(&plan, pdf).unwrap();

        assert_eq!(format, DocumentFormat::PwgRaster);
        assert_eq!(&payload[..4], b"RaS2");
        assert_eq!(&payload[4..13], b"PwgRaster");

        let bad = render(&plan, Bytes::from_static(b"not a pdf"));
        assert!(matches!(bad, Err(PrintError::Render(_))));
    }

    #[tokio::test]
    async fn print_pdf_sends_pdf_directly() {
        let fake = FakePrinter::start(Config::default()).await;
        let pdf = Bytes::from(test_support::pdf(&[A4]));

        print_pdf(&printer_at(&fake, vec!["PDF"]), "doc.pdf", CLIENT, pdf.clone()).await.unwrap();

        let jobs = fake.print_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].document, pdf);
    }

    #[tokio::test]
    async fn print_pdf_converts_for_raster_printers() {
        let fake = FakePrinter::start(Config::raster_only(72, &["srgb_8"])).await;

        print_pdf(&printer_at(&fake, vec!["PWG-Raster"]), "doc.pdf", CLIENT, Bytes::from(test_support::pdf(&[A4])))
            .await
            .unwrap();

        let jobs = fake.print_jobs();
        assert_eq!(jobs[0].attr("document-format").as_deref(), Some("image/pwg-raster"));
        assert_eq!(&jobs[0].document[..4], b"RaS2");
    }

    #[tokio::test]
    async fn print_pdf_fails_for_unsupported_printers() {
        let fake = FakePrinter::start(Config { formats: vec!["image/urf"], ..Config::default() }).await;
        let result = print_pdf(&printer_at(&fake, vec!["URF"]), "doc.pdf", CLIENT, Bytes::new()).await;
        assert!(matches!(result, Err(PrintError::UnsupportedFormat)));
        assert!(fake.print_jobs().is_empty());
    }

    #[tokio::test]
    async fn probe_reports_name_model_and_formats() {
        let fake = FakePrinter::start(Config {
            formats: vec!["application/pdf", "image/urf", "image/jpeg"],
            ..Config::default()
        })
        .await;

        let info = probe_printer(&fake.uri().parse().unwrap()).await.unwrap();

        assert_eq!(info.name.as_deref(), Some("Fake Printer"));
        assert_eq!(info.model.as_deref(), Some("Fake Model 1"));
        assert_eq!(info.formats, ["PDF", "URF"]);
    }

    #[tokio::test]
    async fn probe_falls_back_to_printer_name_and_skips_blank_values() {
        let fake = FakePrinter::start(Config { printer_info: Some(""), model: None, ..Config::default() }).await;
        let info = probe_printer(&fake.uri().parse().unwrap()).await.unwrap();
        assert_eq!(info.name.as_deref(), Some("fake-printer"));
        assert_eq!(info.model, None);

        fake.configure(|c| {
            c.printer_info = None;
            c.printer_name = None;
        });
        assert_eq!(probe_printer(&fake.uri().parse().unwrap()).await.unwrap().name, None);
    }

    #[tokio::test]
    async fn probe_reports_rejections() {
        let fake = FakePrinter::start(Config {
            attributes_status: StatusCode::ClientErrorNotAuthorized,
            ..Config::default()
        })
        .await;
        let result = probe_printer(&fake.uri().parse().unwrap()).await;
        assert!(matches!(result, Err(PrintError::Rejected { status: StatusCode::ClientErrorNotAuthorized, .. })));
    }

    #[tokio::test]
    async fn probe_formats_treats_failure_as_none() {
        test_support::init_tracing();
        let fake = FakePrinter::start(Config::raster_only(300, &["srgb_8"])).await;
        let uri: Uri = fake.uri().parse().unwrap();
        assert_eq!(probe_formats(&uri).await, ["PWG-Raster"]);

        fake.configure(|c| c.online = false);
        assert!(probe_formats(&uri).await.is_empty());
    }

    #[test]
    fn invalid_uris_convert_to_print_errors() {
        let err: PrintError = "not a uri at all".parse::<Uri>().unwrap_err().into();
        assert!(err.to_string().starts_with("invalid printer URI: "));
    }

    #[test]
    fn text_values_render_for_status_messages() {
        let mut response = IppRequestResponse::new_response(IppVersion::v1_1(), StatusCode::SuccessfulOk, 1).unwrap();
        assert_eq!(status_message(&response), None);
        response.attributes_mut().add(
            DelimiterTag::OperationAttributes,
            IppAttribute::with_name("status-message", IppValue::TextWithoutLanguage(IppTextValue::new("ok").unwrap()))
                .unwrap(),
        );
        assert_eq!(status_message(&response).as_deref(), Some("ok"));
    }
}
