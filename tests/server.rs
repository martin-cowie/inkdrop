//! Runs the `inkdrop` server binary end to end: printers configured by URI
//! (fake IPP printers) appear on the event stream, PDFs uploaded to them are
//! printed, and the frontend is served.

mod support;

use std::process::Stdio;
use std::time::Duration;

use futures_util::TryStreamExt;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use support::{A4, Config, FakePrinter, TempDir};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

const WAIT: Duration = Duration::from_secs(10);

fn inkdrop(dir: &TempDir, envs: &[(&str, &str)]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_inkdrop"))
        .current_dir(dir.path())
        .env("PORT", "0")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "inkdrop=info")
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start inkdrop")
}

/// Read log lines until one contains `needle`, returning it.
async fn wait_for_log(lines: &mut Lines<BufReader<ChildStdout>>, needle: &str) -> String {
    tokio::time::timeout(WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains(needle) {
                return line;
            }
        }
        panic!("inkdrop exited before logging {needle:?}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"))
}

/// Follow the printer event stream until `done` accepts a printer list.
async fn wait_for_printers(base: &str, mut done: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
    let response = reqwest::get(format!("{base}/api/printers")).await.unwrap();
    let mut body = response.bytes_stream();
    let mut buffer = String::new();
    tokio::time::timeout(WAIT, async {
        loop {
            while let Some(end) = buffer.find("\n\n") {
                let event: String = buffer.drain(..end + 2).collect();
                if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                    let printers: Vec<Value> = serde_json::from_str(data).unwrap();
                    if done(&printers) {
                        return printers;
                    }
                }
            }
            let chunk = body.try_next().await.unwrap().expect("event stream ended");
            buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    })
    .await
    .expect("timed out waiting for printers")
}

fn find<'a>(printers: &'a [Value], uri: &str) -> Option<&'a Value> {
    printers.iter().find(|p| p["uri"] == uri)
}

async fn upload(base: &str, id: &str) -> reqwest::StatusCode {
    let part = Part::bytes(support::pdf(&[A4])).file_name("upload.pdf").mime_str("application/pdf").unwrap();
    let url = format!("{base}/api/print/{id}");
    reqwest::Client::new().post(url).multipart(Form::new().part("file", part)).send().await.unwrap().status()
}

#[cfg(unix)]
fn terminate(child: &Child) {
    let pid = child.id().expect("still running").to_string();
    assert!(std::process::Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn serves_configured_printers_prints_and_stops_cleanly() {
    let pdf_printer = FakePrinter::start(Config { printer_info: Some("PDF Printer"), ..Config::default() }).await;
    let raster_printer = FakePrinter::start(Config {
        printer_info: Some("Raster Printer"),
        ..Config::raster_only(72, &["sgray_8"])
    })
    .await;
    let dir = TempDir::new("server");
    std::fs::create_dir_all(dir.path().join("frontend/dist")).unwrap();
    std::fs::write(dir.path().join("frontend/dist/index.html"), "<title>inkdrop</title>").unwrap();

    let printers = format!("{}, {}", pdf_printer.uri(), raster_printer.uri());
    let mut child = inkdrop(&dir, &[("INKDROP_PRINTERS", &printers)]);
    let mut logs = BufReader::new(child.stdout.take().unwrap()).lines();
    let listening = wait_for_log(&mut logs, "inkdrop listening").await;
    let port = listening.rsplit(':').next().unwrap().trim();
    assert_ne!(port, "0", "logs the port actually bound: {listening}");
    let base = format!("http://127.0.0.1:{port}");

    // mDNS may list real printers too; only the configured ones matter here.
    let listed = wait_for_printers(&base, |printers| {
        find(printers, &pdf_printer.uri()).is_some() && find(printers, &raster_printer.uri()).is_some()
    })
    .await;
    let pdf_view = find(&listed, &pdf_printer.uri()).unwrap();
    let raster_view = find(&listed, &raster_printer.uri()).unwrap();
    assert_eq!(pdf_view["name"], "PDF Printer");
    assert_eq!(pdf_view["model"], "Fake Model 1");
    assert_eq!(pdf_view["formats"], serde_json::json!(["PDF"]));
    assert_eq!(raster_view["formats"], serde_json::json!(["PWG-Raster"]));

    assert_eq!(upload(&base, pdf_view["id"].as_str().unwrap()).await, 204);
    let jobs = pdf_printer.print_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].attr("document-format").as_deref(), Some("application/pdf"));
    assert_eq!(jobs[0].attr("job-name").as_deref(), Some("upload.pdf"));
    assert_eq!(jobs[0].document, support::pdf(&[A4]));

    assert_eq!(upload(&base, raster_view["id"].as_str().unwrap()).await, 204);
    let jobs = raster_printer.print_jobs();
    assert_eq!(jobs[0].attr("document-format").as_deref(), Some("image/pwg-raster"));
    assert_eq!(&jobs[0].document[..4], b"RaS2");

    assert_eq!(upload(&base, "no-such-printer").await, 404);

    let index = reqwest::get(format!("{base}/")).await.unwrap();
    assert_eq!(index.status(), 200);
    assert_eq!(index.text().await.unwrap(), "<title>inkdrop</title>");

    terminate(&child);
    wait_for_log(&mut logs, "inkdrop stopping").await;
    let status = tokio::time::timeout(WAIT, child.wait()).await.expect("inkdrop should exit").unwrap();
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn refuses_to_start_without_pdfium() {
    let dir = TempDir::new("server");
    let mut child = inkdrop(&dir, &[("PDFIUM_DYNAMIC_LIB_PATH", "/nonexistent")]);
    let mut logs = BufReader::new(child.stdout.take().unwrap()).lines();

    wait_for_log(&mut logs, "PDFium is unavailable").await;
    let status = tokio::time::timeout(WAIT, child.wait()).await.expect("inkdrop should exit").unwrap();
    assert_eq!(status.code(), Some(1));
}
