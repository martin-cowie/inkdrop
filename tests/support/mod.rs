//! Test doubles shared by the unit tests (as `crate::test_support`) and the
//! integration tests (as `mod support`): a fake IPP printer, and a generator
//! for small PDFs.
#![allow(dead_code)]

use std::io::{Cursor, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode as HttpStatus, header};
use axum::response::{IntoResponse, Response};
use inkdrop::notifications::{self as events, Message, Value};
use ipp::parser::IppParser;
use ipp::prelude::*;
use ipp::value::{BoundedString, IppTextValue};
use tokio::sync::watch;

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
    /// `printer-state`.
    pub printer_state: PrinterState,
    /// `printer-state-reasons`; `none` if empty.
    pub printer_state_reasons: Vec<&'static str>,
    /// `printer-state-message`, if any.
    pub printer_state_message: Option<&'static str>,
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
    /// `job-id` of the first job, numbered upwards from there; with `None`,
    /// jobs get no id and aren't queued.
    pub job_id: Option<i32>,
    /// Whether the printer supports event notifications: subscriptions with
    /// the `ippget` pull method.
    pub notifications: bool,
    /// How long Get-Notifications with `notify-wait` waits for an event
    /// before answering without one.
    pub notify_wait: Duration,
    /// Whether a waiting Get-Notifications answers without the events that
    /// woke it, as `ippserver` does, leaving them for the next request.
    pub notify_wakes_empty: bool,
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
            printer_state: PrinterState::Idle,
            printer_state_reasons: Vec::new(),
            printer_state_message: None,
            attributes_status: StatusCode::SuccessfulOk,
            print_status: StatusCode::SuccessfulOk,
            status_message: None,
            job_state: Some(IppValue::Enum(JobState::Pending as i32)),
            job_state_reasons: vec!["none"],
            job_id: Some(42),
            notifications: false,
            notify_wait: Duration::from_millis(500),
            notify_wakes_empty: false,
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
    /// Every attribute group in the request; empty for subscription
    /// operations, whose groups the `ipp` crate can't represent.
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

/// A job in the fake printer's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FakeJob {
    /// The job's `job-id`.
    pub id: i32,
    /// The job's `job-state`.
    pub state: JobState,
}

impl FakeJob {
    fn is_finished(&self) -> bool {
        matches!(self.state, JobState::Canceled | JobState::Aborted | JobState::Completed)
    }
}

#[derive(Default)]
struct Shared {
    config: Config,
    received: Vec<Received>,
    jobs: Vec<FakeJob>,
    jobs_created: i32,
    /// Every event raised, numbered from 1 by position.
    events: Vec<&'static str>,
    subscriptions: Vec<i32>,
    subscriptions_created: i32,
}

struct Fake {
    shared: Mutex<Shared>,
    /// The number of events raised, for Get-Notifications to wait on.
    event_count: watch::Sender<usize>,
}

impl Fake {
    fn raise(&self, shared: &mut Shared, event: &'static str) {
        shared.events.push(event);
        self.event_count.send_replace(shared.events.len());
    }
}

/// An IPP printer on a local port, served over plain HTTP as `ipp://` is.
/// Print-Job queues a job, which stays pending until changed with
/// [`FakePrinter::set_job_state`].
pub struct FakePrinter {
    addr: SocketAddr,
    fake: Arc<Fake>,
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
        Self::start_at(SocketAddr::new(ip, 0), config).await.expect("bind fake printer")
    }

    /// Start on `addr`, answering as `config` says.
    ///
    /// # Errors
    ///
    /// If `addr` can't be bound, e.g. because the port is in use.
    pub async fn start_at(addr: SocketAddr, config: Config) -> std::io::Result<Self> {
        let fake = Arc::new(Fake {
            shared: Mutex::new(Shared { config, ..Shared::default() }),
            event_count: watch::channel(0).0,
        });
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let app = axum::Router::new().fallback(handle).with_state(fake.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(FakePrinter { addr, fake, task })
    }

    /// The port the printer listens on.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The printer's `ipp://` URI.
    pub fn uri(&self) -> String {
        format!("ipp://{}/ipp/print", self.addr)
    }

    /// Apply `change` to the printer's [`Config`], from the next request on,
    /// raising a `printer-state-changed` event.
    pub fn configure(&self, change: impl FnOnce(&mut Config)) {
        let mut shared = self.fake.shared.lock().unwrap();
        change(&mut shared.config);
        self.fake.raise(&mut shared, "printer-state-changed");
    }

    /// The printer's current [`Config`].
    pub fn config(&self) -> Config {
        self.fake.shared.lock().unwrap().config.clone()
    }

    /// Every request received so far, oldest first.
    pub fn received(&self) -> Vec<Received> {
        self.fake.shared.lock().unwrap().received.clone()
    }

    /// The Print-Job requests received so far, oldest first.
    pub fn print_jobs(&self) -> Vec<Received> {
        self.received().into_iter().filter(Received::is_print_job).collect()
    }

    /// Every job the printer knows, in queue order.
    pub fn jobs(&self) -> Vec<FakeJob> {
        self.fake.shared.lock().unwrap().jobs.clone()
    }

    /// Move job `id` to `state`, raising `job-state-changed`, and
    /// `job-completed` too if the job has finished.
    ///
    /// # Panics
    ///
    /// If there is no such job.
    pub fn set_job_state(&self, id: i32, state: JobState) {
        let mut shared = self.fake.shared.lock().unwrap();
        if Self::change_job(&mut shared, id, state) {
            self.fake.raise(&mut shared, "job-completed");
        }
        self.fake.raise(&mut shared, "job-state-changed");
    }

    /// Move job `id` to `state` without raising any event, as printers
    /// sometimes fail to.
    ///
    /// # Panics
    ///
    /// If there is no such job.
    pub fn set_job_state_quietly(&self, id: i32, state: JobState) {
        Self::change_job(&mut self.fake.shared.lock().unwrap(), id, state);
    }

    /// Returns whether the job has finished.
    fn change_job(shared: &mut Shared, id: i32, state: JobState) -> bool {
        let job = shared.jobs.iter_mut().find(|job| job.id == id).expect("no such job");
        job.state = state;
        job.is_finished()
    }

    /// Forget job `id`, as printers that keep little history do, so that
    /// Get-Job-Attributes no longer finds it.
    pub fn forget_job(&self, id: i32) {
        self.fake.shared.lock().unwrap().jobs.retain(|job| job.id != id);
    }

    /// The ids of the event subscriptions currently open.
    pub fn subscriptions(&self) -> Vec<i32> {
        self.fake.shared.lock().unwrap().subscriptions.clone()
    }

    /// Drop every subscription, as a printer does when it restarts.
    pub fn drop_subscriptions(&self) {
        self.fake.shared.lock().unwrap().subscriptions.clear();
    }

    /// How many requests of IPP operation `operation` have been received.
    pub fn count(&self, operation: u16) -> usize {
        self.received().iter().filter(|request| request.operation as u16 == operation).count()
    }
}

impl Drop for FakePrinter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    let operation = body.get(2..4).map(|code| u16::from_be_bytes([code[0], code[1]]));
    if let Some(operation @ (events::CREATE_PRINTER_SUBSCRIPTIONS | events::CANCEL_SUBSCRIPTION | events::GET_NOTIFICATIONS)) = operation {
        return handle_subscription(&fake, operation, &body).await;
    }

    let request = match IppParser::new(Cursor::new(body.to_vec())).parse() {
        Ok(request) => request,
        Err(err) => return (HttpStatus::BAD_REQUEST, err.to_string()).into_response(),
    };
    let header = *request.header();
    let attributes = request.attributes().clone();
    let mut document = Vec::new();
    request.into_payload().read_to_end(&mut document).unwrap();

    let mut shared = fake.shared.lock().unwrap();
    shared.received.push(Received { operation: header.operation_or_status, attributes: attributes.clone(), document });
    let config = shared.config.clone();
    if !config.online {
        return (HttpStatus::SERVICE_UNAVAILABLE, "offline").into_response();
    }

    let operation = header.operation_or_status;
    let operation_attr = |name: &str| {
        attributes.first_of(DelimiterTag::OperationAttributes).and_then(|group| group.get(name)).map(|attr| attr.value().clone())
    };
    let status = if operation == Operation::PrintJob as i16 {
        config.print_status
    } else if operation == Operation::GetPrinterAttributes as i16 {
        config.attributes_status
    } else {
        StatusCode::SuccessfulOk
    };
    let mut response = IppRequestResponse::new_response(header.version, status, header.request_id).unwrap();
    if let Some(message) = config.status_message {
        response.attributes_mut().add(DelimiterTag::OperationAttributes, attr("status-message", text(message)));
    }

    if operation == Operation::PrintJob as i16 {
        if status.is_success() {
            print_job(&fake, &mut shared, &mut response);
        }
    } else if operation == Operation::GetJobs as i16 {
        let completed = operation_attr("which-jobs").is_some_and(|which| which.to_string() == "completed");
        for job in shared.jobs.iter().filter(|job| job.is_finished() == completed) {
            response.attributes_mut().groups_mut().push(job_group(job));
        }
    } else if operation == Operation::GetJobAttributes as i16 {
        let id = operation_attr("job-id").and_then(|id| id.as_integer().copied());
        match shared.jobs.iter().find(|job| Some(job.id) == id) {
            Some(job) => response.attributes_mut().groups_mut().push(job_group(job)),
            None => response.header_mut().operation_or_status = StatusCode::ClientErrorNotFound as i16,
        }
    } else if operation == Operation::GetPrinterAttributes as i16 {
        printer_attributes(&config, &shared.jobs, &mut response);
    } else {
        response.header_mut().operation_or_status = StatusCode::ServerErrorOperationNotSupported as i16;
    }

    ([(header::CONTENT_TYPE, "application/ipp")], response.to_bytes()).into_response()
}

fn print_job(fake: &Fake, shared: &mut Shared, response: &mut IppRequestResponse) {
    let config = shared.config.clone();
    let attrs = response.attributes_mut();
    if let Some(first) = config.job_id {
        let id = first + shared.jobs_created;
        shared.jobs_created += 1;
        shared.jobs.push(FakeJob { id, state: JobState::Pending });
        fake.raise(shared, "job-created");
        attrs.add(DelimiterTag::JobAttributes, attr("job-id", IppValue::Integer(id)));
    }
    if let Some(state) = config.job_state {
        attrs.add(DelimiterTag::JobAttributes, attr("job-state", state));
    }
    if !config.job_state_reasons.is_empty() {
        attrs.add(DelimiterTag::JobAttributes, attr("job-state-reasons", array(&config.job_state_reasons, keyword)));
    }
}

fn job_group(job: &FakeJob) -> IppAttributeGroup {
    let mut result = IppAttributeGroup::new(DelimiterTag::JobAttributes);
    result.attributes_mut().extend([
        attr("job-id", IppValue::Integer(job.id)),
        attr("job-state", IppValue::Enum(job.state as i32)),
        attr("job-state-reasons", keyword("none")),
    ]);
    result
}

fn printer_attributes(config: &Config, jobs: &[FakeJob], response: &mut IppRequestResponse) {
    let printer = DelimiterTag::PrinterAttributes;
    let attrs = response.attributes_mut();
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

    attrs.add(printer, attr("printer-state", IppValue::Enum(config.printer_state as i32)));
    let reasons = if config.printer_state_reasons.is_empty() { vec!["none"] } else { config.printer_state_reasons.clone() };
    attrs.add(printer, attr("printer-state-reasons", array(&reasons, keyword)));
    if let Some(message) = config.printer_state_message {
        attrs.add(printer, attr("printer-state-message", text(message)));
    }
    let queued = jobs.iter().filter(|job| !job.is_finished()).count();
    attrs.add(printer, attr("queued-job-count", IppValue::Integer(queued as i32)));
    if config.notifications {
        attrs.add(printer, attr("notify-pull-method-supported", keyword("ippget")));
    }
}

/// Answer Create-Printer-Subscriptions, Cancel-Subscription or
/// Get-Notifications, which the `ipp` crate can't parse.
async fn handle_subscription(fake: &Fake, operation: u16, body: &[u8]) -> Response {
    let Ok(request) = Message::decode(body) else {
        return (HttpStatus::BAD_REQUEST, "malformed IPP request").into_response();
    };
    let operation_attr = |name: &str| {
        request.groups_of(events::OPERATION_ATTRIBUTES).find_map(|group| group.first(name)).and_then(Value::as_integer)
    };
    let config = {
        let mut shared = fake.shared.lock().unwrap();
        shared.received.push(Received { operation: operation as i16, attributes: IppAttributes::new(), document: Vec::new() });
        shared.config.clone()
    };
    if !config.online {
        return (HttpStatus::SERVICE_UNAVAILABLE, "offline").into_response();
    }
    if !config.notifications {
        return ipp_response(Message::response(events::SERVER_ERROR_OPERATION_NOT_SUPPORTED, request.request_id));
    }

    let response = match operation {
        events::CREATE_PRINTER_SUBSCRIPTIONS => {
            let mut shared = fake.shared.lock().unwrap();
            shared.subscriptions_created += 1;
            let id = shared.subscriptions_created;
            shared.subscriptions.push(id);
            let mut response = Message::response(0, request.request_id);
            response.add(events::SUBSCRIPTION_ATTRIBUTES, "notify-subscription-id", vec![Value::integer(id)]);
            response
        }
        events::CANCEL_SUBSCRIPTION => {
            let id = operation_attr("notify-subscription-id");
            fake.shared.lock().unwrap().subscriptions.retain(|subscription| Some(*subscription) != id);
            Message::response(0, request.request_id)
        }
        _ => {
            let id = operation_attr("notify-subscription-ids");
            let first = operation_attr("notify-sequence-numbers").unwrap_or(1).max(1) as usize;
            let wait = request
                .groups_of(events::OPERATION_ATTRIBUTES)
                .find_map(|group| group.first("notify-wait"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            notifications(fake, id, first, wait, &config, request.request_id).await
        }
    };
    ipp_response(response)
}

async fn notifications(fake: &Fake, id: Option<i32>, first: usize, wait: bool, config: &Config, request_id: i32) -> Message {
    let mut event_count = fake.event_count.subscribe();
    let mut waited = !wait;
    let mut woken = false;
    loop {
        let (known, pending) = {
            let shared = fake.shared.lock().unwrap();
            let known = id.is_some_and(|id| shared.subscriptions.contains(&id));
            let pending: Vec<(usize, &'static str)> =
                shared.events.iter().enumerate().map(|(index, event)| (index + 1, *event)).filter(|(sequence, _)| *sequence >= first).collect();
            (known, pending)
        };
        if !known {
            return Message::response(events::CLIENT_ERROR_NOT_FOUND, request_id);
        }
        if woken && config.notify_wakes_empty {
            return Message::response(0, request_id);
        }
        if !pending.is_empty() || waited {
            let mut response = Message::response(0, request_id);
            response.add(events::OPERATION_ATTRIBUTES, "notify-get-interval", vec![Value::integer(30)]);
            for (sequence, event) in pending {
                response.start_group(events::EVENT_NOTIFICATION_ATTRIBUTES);
                response.add(events::EVENT_NOTIFICATION_ATTRIBUTES, "notify-subscription-id", vec![Value::integer(id.unwrap_or_default())]);
                response.add(events::EVENT_NOTIFICATION_ATTRIBUTES, "notify-sequence-number", vec![Value::integer(sequence as i32)]);
                response.add(events::EVENT_NOTIFICATION_ATTRIBUTES, "notify-subscribed-event", vec![Value::keyword(event)]);
            }
            return response;
        }
        woken = tokio::time::timeout(config.notify_wait, event_count.changed()).await.is_ok();
        waited = true;
    }
}

fn ipp_response(message: Message) -> Response {
    ([(header::CONTENT_TYPE, "application/ipp")], message.encode()).into_response()
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

/// Monitor timings short enough for tests.
pub fn quick_timings() -> inkdrop::status::Timings {
    inkdrop::status::Timings {
        idle_poll: Duration::from_millis(100),
        busy_poll: Duration::from_millis(50),
        min_gap: Duration::from_millis(10),
        viewer_check: Duration::from_millis(10),
        lease: Duration::from_secs(60),
        request_timeout: Duration::from_secs(5),
        retry_streaming: Duration::from_millis(300),
    }
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
