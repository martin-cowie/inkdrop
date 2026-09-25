//! Test doubles shared by the unit tests (as `crate::test_support`) and the
//! integration tests (as `mod support`): a fake IPP printer, and a generator
//! for small PDFs.
#![allow(dead_code)]

use std::io::{Cursor, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode as HttpStatus, header};
use axum::response::{IntoResponse, Response};
use ipp::parser::IppParser;
use ipp::prelude::*;
use ipp::value::{BoundedString, IppTextValue};

/// How the fake printer describes itself and answers requests. Changes made
/// with [`FakePrinter::configure`] apply to the next request.
#[derive(Clone, Debug)]
pub struct Config {
    /// `document-format-supported`.
    pub formats: Vec<&'static str>,
    /// `pwg-raster-document-resolution-supported`, in dots per inch.
    pub raster_resolutions: Vec<i32>,
    /// `pwg-raster-document-type-supported`, e.g. `srgb_8`.
    pub raster_types: Vec<&'static str>,
    /// `printer-name`, if any.
    pub printer_name: Option<&'static str>,
    /// `printer-info`, if any.
    pub printer_info: Option<&'static str>,
    /// `printer-make-and-model`, if any.
    pub model: Option<&'static str>,
    /// IPP status for Get-Printer-Attributes.
    pub attributes_status: StatusCode,
    /// IPP status for Print-Job.
    pub print_status: StatusCode,
    /// `status-message` for every response, if any.
    pub status_message: Option<&'static str>,
    /// `job-state` in the Print-Job response, if any.
    pub job_state: Option<IppValue>,
    /// `job-state-reasons` in the Print-Job response; omitted if empty.
    pub job_state_reasons: Vec<&'static str>,
    /// `job-id` in the Print-Job response, if any.
    pub job_id: Option<i32>,
    /// When false, every request fails with HTTP 503 instead of an IPP answer.
    pub online: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            formats: vec!["application/pdf"],
            raster_resolutions: Vec::new(),
            raster_types: Vec::new(),
            printer_name: Some("fake-printer"),
            printer_info: Some("Fake Printer"),
            model: Some("Fake Model 1"),
            attributes_status: StatusCode::SuccessfulOk,
            print_status: StatusCode::SuccessfulOk,
            status_message: None,
            job_state: Some(IppValue::Enum(JobState::Pending as i32)),
            job_state_reasons: vec!["none"],
            job_id: Some(42),
            online: true,
        }
    }
}

impl Config {
    /// A printer that only takes PWG-Raster, at `dpi`, in the given types.
    pub fn raster_only(dpi: i32, types: &[&'static str]) -> Self {
        Config {
            formats: vec!["image/pwg-raster"],
            raster_resolutions: vec![dpi],
            raster_types: types.to_vec(),
            ..Config::default()
        }
    }
}

/// One IPP request the fake printer received.
#[derive(Clone, Debug)]
pub struct Received {
    /// The IPP operation id.
    pub operation: i16,
    /// Every attribute group in the request.
    pub attributes: IppAttributes,
    /// The document data following the attributes (empty for queries).
    pub document: Vec<u8>,
}

impl Received {
    /// Whether this was a Print-Job request.
    pub fn is_print_job(&self) -> bool {
        self.operation == Operation::PrintJob as i16
    }

    /// The value of operation attribute `name`, as a string.
    pub fn attr(&self, name: &str) -> Option<String> {
        self.attributes
            .first_of(DelimiterTag::OperationAttributes)
            .and_then(|group| group.get(name))
            .map(|attr| attr.value().to_string())
    }
}

#[derive(Default)]
struct Shared {
    config: Config,
    received: Vec<Received>,
}

/// An IPP printer on a local port, served over plain HTTP as `ipp://` is.
pub struct FakePrinter {
    addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakePrinter {
    /// Start on 127.0.0.1 with an OS-chosen port, answering as `config` says.
    ///
    /// # Panics
    ///
    /// If it can't bind a port.
    pub async fn start(config: Config) -> Self {
        Self::start_on(IpAddr::V4(Ipv4Addr::LOCALHOST), config).await
    }

    /// Start on `ip` (e.g. `0.0.0.0` to be reachable at a LAN address) with
    /// an OS-chosen port, answering as `config` says.
    ///
    /// # Panics
    ///
    /// If it can't bind a port.
    pub async fn start_on(ip: IpAddr, config: Config) -> Self {
        let shared = Arc::new(Mutex::new(Shared { config, received: Vec::new() }));
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, 0)).await.expect("bind fake printer");
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(handle).with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakePrinter { addr, shared, task }
    }

    /// The port the printer listens on.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The printer's `ipp://` URI.
    pub fn uri(&self) -> String {
        format!("ipp://{}/ipp/print", self.addr)
    }

    /// Apply `change` to the printer's [`Config`], from the next request on.
    pub fn configure(&self, change: impl FnOnce(&mut Config)) {
        change(&mut self.shared.lock().unwrap().config);
    }

    /// Every request received so far, oldest first.
    pub fn received(&self) -> Vec<Received> {
        self.shared.lock().unwrap().received.clone()
    }

    /// The Print-Job requests received so far, oldest first.
    pub fn print_jobs(&self) -> Vec<Received> {
        self.received().into_iter().filter(Received::is_print_job).collect()
    }
}

impl Drop for FakePrinter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(State(shared): State<Arc<Mutex<Shared>>>, body: Bytes) -> Response {
    let request = match IppParser::new(Cursor::new(body.to_vec())).parse() {
        Ok(request) => request,
        Err(err) => return (HttpStatus::BAD_REQUEST, err.to_string()).into_response(),
    };
    let header = *request.header();
    let attributes = request.attributes().clone();
    let mut document = Vec::new();
    request.into_payload().read_to_end(&mut document).unwrap();

    let mut shared = shared.lock().unwrap();
    shared.received.push(Received { operation: header.operation_or_status, attributes, document });
    let config = shared.config.clone();
    drop(shared);

    if !config.online {
        return (HttpStatus::SERVICE_UNAVAILABLE, "offline").into_response();
    }

    let is_print_job = header.operation_or_status == Operation::PrintJob as i16;
    let status = if is_print_job { config.print_status } else { config.attributes_status };
    let mut response = IppRequestResponse::new_response(header.version, status, header.request_id).unwrap();
    let attrs = response.attributes_mut();

    if let Some(message) = config.status_message {
        attrs.add(DelimiterTag::OperationAttributes, attr("status-message", text(message)));
    }

    if is_print_job {
        if let Some(id) = config.job_id {
            attrs.add(DelimiterTag::JobAttributes, attr("job-id", IppValue::Integer(id)));
        }
        if let Some(state) = config.job_state {
            attrs.add(DelimiterTag::JobAttributes, attr("job-state", state));
        }
        if !config.job_state_reasons.is_empty() {
            attrs.add(DelimiterTag::JobAttributes, attr("job-state-reasons", array(&config.job_state_reasons, keyword)));
        }
    } else {
        let printer = DelimiterTag::PrinterAttributes;
        attrs.add(printer, attr("document-format-supported", array(&config.formats, mime)));
        if let Some(name) = config.printer_name {
            attrs.add(printer, attr("printer-name", IppValue::NameWithoutLanguage(BoundedString::new(name).unwrap())));
        }
        if let Some(info) = config.printer_info {
            attrs.add(printer, attr("printer-info", text(info)));
        }
        if let Some(model) = config.model {
            attrs.add(printer, attr("printer-make-and-model", text(model)));
        }
        if !config.raster_resolutions.is_empty() {
            let resolutions = config
                .raster_resolutions
                .iter()
                .map(|&dpi| IppValue::Resolution { cross_feed: dpi, feed: dpi, units: 3 })
                .collect();
            attrs.add(printer, attr("pwg-raster-document-resolution-supported", IppValue::Array(resolutions)));
        }
        if !config.raster_types.is_empty() {
            attrs.add(printer, attr("pwg-raster-document-type-supported", array(&config.raster_types, keyword)));
        }
    }

    ([(header::CONTENT_TYPE, "application/ipp")], response.to_bytes()).into_response()
}

fn attr(name: &str, value: IppValue) -> IppAttribute {
    IppAttribute::with_name(name, value).unwrap()
}

fn text(value: &str) -> IppValue {
    IppValue::TextWithoutLanguage(IppTextValue::new(value).unwrap())
}

fn keyword(value: &str) -> IppValue {
    IppValue::Keyword(BoundedString::new(value).unwrap())
}

fn mime(value: &str) -> IppValue {
    IppValue::MimeMediaType(BoundedString::new(value).unwrap())
}

/// A single value when there's one, otherwise an array, as printers send.
fn array(values: &[&str], make: fn(&str) -> IppValue) -> IppValue {
    match values {
        [single] => make(single),
        many => IppValue::Array(many.iter().map(|v| make(v)).collect()),
    }
}

/// A PDF with one page per `(width, height)` in points, each with a red
/// square near the bottom-left corner on a white background.
pub fn pdf(pages: &[(f32, f32)]) -> Vec<u8> {
    let page_count = pages.len();
    let kids: Vec<String> = (0..page_count).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!("<< /Type /Pages /Kids [{}] /Count {page_count} >>", kids.join(" ")),
    ];
    for (i, (width, height)) in pages.iter().enumerate() {
        let content = "1 0 0 rg 10 10 20 20 re f";
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Contents {} 0 R >>",
            4 + 2 * i
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
    );
    out
}

/// An A4 page, in points.
pub const A4: (f32, f32) = (595.0, 842.0);

/// Log everything, to the test harness's captured output, so that logging
/// statements in the code under test run (and count towards coverage).
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt().with_env_filter("inkdrop=trace").with_test_writer().try_init();
}

/// A fresh directory under the system temp dir, removed when dropped.
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    /// Create a directory whose name includes `label`.
    ///
    /// # Panics
    ///
    /// If the directory can't be created.
    pub fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("inkdrop-{label}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    /// The directory's path.
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
