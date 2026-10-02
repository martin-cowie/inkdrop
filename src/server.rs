//! The web server: an SSE stream of discovered printers and their statuses,
//! an upload endpoint that prints a PDF, and the static frontend.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Serialize;
use tokio_stream::wrappers::WatchStream;
use tower_http::services::ServeDir;
use tracing::{error, info};

use crate::discovery::{Printer, Registry};
use crate::printing::{self, PrintError};
use crate::status::{Monitor, PrinterStatus};

const MAX_UPLOAD_BYTES: usize = 200 * 1024 * 1024;

/// State shared by the request handlers.
#[derive(Clone)]
pub struct AppState {
    /// The printers to list and print to.
    pub registry: Registry,
    /// The printers' statuses, and the jobs to follow.
    pub monitor: Monitor,
}

/// The API routes, backed by `state`, with everything else served as static
/// files from `frontend_dir`.
pub fn router(state: AppState, frontend_dir: impl AsRef<std::path::Path>) -> Router {
    Router::new()
        .route("/api/printers", get(printers_stream))
        .route(
            "/api/print/{id}",
            post(print_handler).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .with_state(state)
        .fallback_service(ServeDir::new(frontend_dir))
}

/// The printer URIs in `list`, a comma-separated list such as
/// `INKDROP_PRINTERS`, trimmed and without empty entries.
pub fn configured_printers(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|uri| !uri.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Serialize)]
struct PrinterView {
    id: String,
    name: String,
    uri: String,
    model: Option<String>,
    formats: Vec<&'static str>,
    status: Option<PrinterStatus>,
}

fn to_views(printers: &HashMap<String, Printer>, statuses: &HashMap<String, PrinterStatus>) -> Vec<PrinterView> {
    let mut result: Vec<PrinterView> = printers
        .values()
        .map(|p| PrinterView {
            id: p.id.clone(),
            name: p.name.clone(),
            uri: p.uri.to_string(),
            model: p.model.clone(),
            formats: p.formats.clone(),
            status: statuses.get(&p.id).cloned(),
        })
        .collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// `GET /api/printers`: a server-sent event stream carrying the printers in
/// `state`, with their statuses, as a JSON array sorted by name: first on
/// connecting, then after every change. Printers are only contacted for
/// their statuses while at least one such stream is open.
pub async fn printers_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let statuses = state.monitor.statuses().subscribe();
    let changes = stream::select(
        WatchStream::new(state.registry.subscribe()).map(|_| ()),
        WatchStream::new(statuses.clone()).map(|_| ()),
    );
    let mut last_sent = None;
    let stream = changes.filter_map(move |()| {
        let views = to_views(&state.registry.borrow(), &statuses.borrow());
        let json = serde_json::to_string(&views).unwrap_or_else(|_| "[]".to_owned());
        let event = (last_sent.as_ref() != Some(&json)).then(|| Ok(Event::default().data(&json)));
        last_sent = Some(json);
        std::future::ready(event)
    });

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// The response to a print request.
#[derive(Serialize)]
pub struct Printed {
    /// The printer's id for the new job, if it gave one.
    #[serde(rename = "jobId")]
    pub job_id: Option<i32>,
}

/// `POST /api/print/{id}`: print the PDF in the first field of `multipart`
/// on the printer `id`, and follow the job in the printer's status.
/// `client_addr` is logged with the job.
///
/// Returns the job id once the printer accepts the job.
///
/// # Errors
///
/// A status and message: 404 if there is no such printer, 400 if the upload
/// is missing or malformed, 415 if it isn't `application/pdf`, 422 if the
/// printer takes neither PDF nor PWG-Raster, and 502 if printing fails.
pub async fn print_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    mut multipart: Multipart,
) -> Result<Json<Printed>, (StatusCode, String)> {
    let printer = {
        let printers = state.registry.borrow();
        printers.get(&id).cloned()
    }
    .ok_or((StatusCode::NOT_FOUND, "printer not found".to_owned()))?;

    let field = multipart
        .next_field()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?
        .ok_or((StatusCode::BAD_REQUEST, "no file provided".to_owned()))?;

    let content_type = field.content_type().unwrap_or_default().to_owned();
    if content_type != "application/pdf" {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "only application/pdf is supported".to_owned(),
        ));
    }

    let job_title = field.file_name().unwrap_or("document.pdf").to_owned();

    let document = field
        .bytes()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;

    info!(
        client_ip = %client_addr.ip(),
        printer_id = %id,
        job_title = %job_title,
        bytes = document.len(),
        "received PDF print job"
    );

    let submitted = printing::print_pdf(&printer, &job_title, client_addr.ip(), document)
        .await
        .map_err(|err| match err {
            PrintError::UnsupportedFormat => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "printer no longer supports PDF".to_owned(),
            ),
            other => {
                error!(client_ip = %client_addr.ip(), job_title = %job_title, error = %other, "print job failed");
                (StatusCode::BAD_GATEWAY, "printer rejected the job".to_owned())
            }
        })?;

    if let Some(job_id) = submitted.job_id {
        state.monitor.job_submitted(&id, job_id);
    }
    Ok(Json(Printed { job_id: submitted.job_id }))
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;
    use reqwest::multipart::{Form, Part};
    use tokio::sync::watch;

    use super::*;
    use crate::test_support::{self, A4, Config, FakePrinter};

    /// The JSON events of a `/api/printers` stream.
    struct Events {
        body: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
        buffer: String,
    }

    impl Events {
        async fn open(base: &str) -> Self {
            let response = reqwest::get(format!("{base}/api/printers")).await.unwrap();
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            Events { body: Box::pin(response.bytes_stream()), buffer: String::new() }
        }

        async fn next(&mut self) -> serde_json::Value {
            loop {
                if let Some(end) = self.buffer.find("\n\n") {
                    let event: String = self.buffer.drain(..end + 2).collect();
                    if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                        return serde_json::from_str(data).unwrap();
                    }
                    continue;
                }
                let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), self.body.try_next())
                    .await
                    .expect("timed out waiting for an event")
                    .unwrap()
                    .expect("stream ended");
                self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        }

        /// Skip events until `done` accepts one, returning it.
        async fn until(&mut self, mut done: impl FnMut(&serde_json::Value) -> bool) -> serde_json::Value {
            loop {
                let event = self.next().await;
                if done(&event) {
                    return event;
                }
            }
        }
    }

    fn printer(id: &str, name: &str, uri: &str, formats: Vec<&'static str>) -> Printer {
        Printer {
            id: id.to_owned(),
            name: name.to_owned(),
            uri: uri.parse().unwrap(),
            model: Some("Model".to_owned()),
            formats,
        }
    }

    /// Serve the app on a local port, returning its base URL.
    async fn serve(registry: Registry, frontend_dir: &std::path::Path) -> String {
        test_support::init_tracing();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let monitor = Monitor::spawn(&registry, test_support::quick_timings());
        let app = router(AppState { registry, monitor }, frontend_dir);
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn serve_printer(printer: Printer) -> String {
        let registry = watch::channel(HashMap::from([(printer.id.clone(), printer)])).0;
        serve(registry, std::path::Path::new("does-not-exist")).await
    }

    fn pdf_part(file_name: Option<&str>) -> Part {
        let part = Part::bytes(test_support::pdf(&[A4])).mime_str("application/pdf").unwrap();
        match file_name {
            Some(name) => part.file_name(name.to_owned()),
            None => part,
        }
    }

    async fn post(base: &str, id: &str, form: Form) -> (u16, String) {
        let response = reqwest::Client::new().post(format!("{base}/api/print/{id}")).multipart(form).send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[test]
    fn configured_printers_are_comma_separated() {
        assert_eq!(configured_printers(" ipp://a/ipp/print, ,ipp://b:1631/x,"), ["ipp://a/ipp/print", "ipp://b:1631/x"]);
        assert!(configured_printers("").is_empty());
    }

    #[test]
    fn views_are_sorted_by_name() {
        let printers = HashMap::from([
            ("2".to_owned(), printer("2", "Zebra", "ipp://z/ipp/print", vec!["PDF"])),
            ("1".to_owned(), printer("1", "Alpha", "ipp://a/ipp/print", vec!["URF", "PWG-Raster"])),
        ]);
        let json = serde_json::to_value(to_views(&printers, &HashMap::new())).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"id": "1", "name": "Alpha", "uri": "ipp://a/ipp/print", "model": "Model", "formats": ["URF", "PWG-Raster"], "status": null},
                {"id": "2", "name": "Zebra", "uri": "ipp://z/ipp/print", "model": "Model", "formats": ["PDF"], "status": null},
            ])
        );
    }

    #[tokio::test]
    async fn printers_stream_sends_the_list_and_every_change() {
        let registry = watch::channel(HashMap::new()).0;
        let base = serve(registry.clone(), std::path::Path::new("does-not-exist")).await;

        let mut events = Events::open(&base).await;
        assert_eq!(events.next().await, serde_json::json!([]));

        registry.send_modify(|map| {
            map.insert("p".to_owned(), printer("p", "Office", "ipp://127.0.0.1:1/ipp/print", vec!["PDF"]));
        });
        let update = events.next().await;
        assert_eq!(update[0]["name"], "Office");
        assert_eq!(update[0]["formats"], serde_json::json!(["PDF"]));

        let unreachable = events.until(|printers| !printers[0]["status"].is_null()).await;
        assert_eq!(unreachable[0]["status"]["state"], "unreachable");
    }

    #[tokio::test]
    async fn printers_stream_follows_the_printer_state_and_submitted_jobs() {
        let fake = FakePrinter::start(Config { notifications: true, ..Config::default() }).await;
        let base = serve_printer(printer("p", "Fake", &fake.uri(), vec!["PDF"])).await;
        let mut events = Events::open(&base).await;
        let ready = events.until(|printers| !printers[0]["status"].is_null()).await;
        assert_eq!(
            ready[0]["status"],
            serde_json::json!({"state": "idle", "reasons": [], "message": null, "queued": 0, "jobs": [], "finished": []})
        );

        let (status, body) = post(&base, "p", Form::new().part("file", pdf_part(Some("a.pdf")))).await;
        assert_eq!((status, body.as_str()), (200, r#"{"jobId":42}"#));
        let queued = events.until(|printers| printers[0]["status"]["queued"] == 1).await;
        assert_eq!(queued[0]["status"]["jobs"], serde_json::json!([{"id": 42, "state": "pending"}]));

        fake.configure(|c| {
            c.printer_state = ipp::model::PrinterState::Stopped;
            c.printer_state_reasons = vec!["media-empty-error"];
            c.printer_state_message = Some("Load paper");
        });
        let stopped = events.until(|printers| printers[0]["status"]["state"] == "stopped").await;
        assert_eq!(stopped[0]["status"]["reasons"], serde_json::json!(["media-empty-error"]));
        assert_eq!(stopped[0]["status"]["message"], "Load paper");

        fake.set_job_state(42, ipp::model::JobState::Completed);
        let finished = events.until(|printers| printers[0]["status"]["finished"] != serde_json::json!([])).await;
        assert_eq!(finished[0]["status"]["finished"], serde_json::json!([{"id": 42, "state": "completed"}]));
        assert_eq!(finished[0]["status"]["jobs"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn printing_a_pdf_sends_it_to_the_printer() {
        let fake = FakePrinter::start(Config::default()).await;
        let base = serve_printer(printer("p", "Fake", &fake.uri(), vec!["PDF"])).await;

        let (status, body) = post(&base, "p", Form::new().part("file", pdf_part(Some("report.pdf")))).await;
        assert_eq!((status, body.as_str()), (200, r#"{"jobId":42}"#));
        let (status, body) = post(&base, "p", Form::new().part("file", pdf_part(None))).await;
        assert_eq!((status, body.as_str()), (200, r#"{"jobId":43}"#));
        fake.configure(|c| c.job_id = None);
        let (status, body) = post(&base, "p", Form::new().part("file", pdf_part(None))).await;
        assert_eq!((status, body.as_str()), (200, r#"{"jobId":null}"#));

        let jobs = fake.print_jobs();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].attr("job-name").as_deref(), Some("report.pdf"));
        assert_eq!(jobs[1].attr("job-name").as_deref(), Some("document.pdf"));
        assert_eq!(jobs[0].document, test_support::pdf(&[A4]));
    }

    #[tokio::test]
    async fn unknown_printers_are_not_found() {
        let base = serve_printer(printer("p", "Fake", "ipp://127.0.0.1:1/ipp/print", vec!["PDF"])).await;
        let (status, body) = post(&base, "nope", Form::new().part("file", pdf_part(Some("a.pdf")))).await;
        assert_eq!((status, body.as_str()), (404, "printer not found"));
    }

    #[tokio::test]
    async fn uploads_must_be_a_pdf() {
        let base = serve_printer(printer("p", "Fake", "ipp://127.0.0.1:1/ipp/print", vec!["PDF"])).await;

        let text = Part::text("hello").file_name("a.txt").mime_str("text/plain").unwrap();
        let (status, body) = post(&base, "p", Form::new().part("file", text)).await;
        assert_eq!((status, body.as_str()), (415, "only application/pdf is supported"));

        let untyped = Part::bytes(b"%PDF".to_vec());
        let (status, _) = post(&base, "p", Form::new().part("file", untyped)).await;
        assert_eq!(status, 415);
    }

    #[tokio::test]
    async fn malformed_uploads_are_bad_requests() {
        let base = serve_printer(printer("p", "Fake", "ipp://127.0.0.1:1/ipp/print", vec!["PDF"])).await;
        let send = async |body: &'static str| {
            let response = reqwest::Client::new()
                .post(format!("{base}/api/print/p"))
                .header("content-type", "multipart/form-data; boundary=x")
                .body(body)
                .send()
                .await
                .unwrap();
            (response.status().as_u16(), response.text().await.unwrap())
        };

        assert_eq!(send("--x--\r\n").await, (400, "no file provided".to_owned()));
        assert_eq!(send("garbage").await.0, 400, "no boundary at all");
        let truncated = "--x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\n\
                         Content-Type: application/pdf\r\n\r\n%PDF";
        assert_eq!(send(truncated).await.0, 400, "file cut short");
    }

    #[tokio::test]
    async fn printer_failures_map_to_http_errors() {
        let fake = FakePrinter::start(Config { formats: vec!["image/urf"], ..Config::default() }).await;
        let base = serve_printer(printer("p", "Fake", &fake.uri(), vec!["URF"])).await;
        let (status, body) = post(&base, "p", Form::new().part("file", pdf_part(Some("a.pdf")))).await;
        assert_eq!((status, body.as_str()), (422, "printer no longer supports PDF"));

        fake.configure(|c| {
            c.formats = vec!["application/pdf"];
            c.print_status = ipp::model::StatusCode::ServerErrorBusy;
        });
        let (status, body) = post(&base, "p", Form::new().part("file", pdf_part(Some("a.pdf")))).await;
        assert_eq!((status, body.as_str()), (502, "printer rejected the job"));
    }

    #[tokio::test]
    async fn other_paths_serve_the_frontend() {
        let dir = test_support::TempDir::new("frontend");
        std::fs::write(dir.path().join("index.html"), "<h1>inkdrop</h1>").unwrap();
        let base = serve(watch::channel(HashMap::new()).0, dir.path()).await;

        let index = reqwest::get(format!("{base}/")).await.unwrap();
        assert_eq!(index.status(), 200);
        assert_eq!(index.text().await.unwrap(), "<h1>inkdrop</h1>");
        assert_eq!(reqwest::get(format!("{base}/missing.js")).await.unwrap().status(), 404);
    }
}
