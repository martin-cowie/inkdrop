//! Runs the `inkdrop-print` binary against a fake IPP printer.

mod support;

use std::path::PathBuf;
use std::process::Output;

use ipp::prelude::*;
use support::{A4, Config, FakePrinter, TempDir};

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn inkdrop_print(args: &[&str], envs: &[(&str, &str)]) -> Run {
    let Output { status, stdout, stderr } = tokio::process::Command::new(env!("CARGO_BIN_EXE_inkdrop-print"))
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .envs(envs.iter().copied())
        .output()
        .await
        .expect("run inkdrop-print");
    Run {
        success: status.success(),
        stdout: String::from_utf8(stdout).unwrap(),
        stderr: String::from_utf8(stderr).unwrap(),
    }
}

fn write_pdf(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("letter.pdf");
    std::fs::write(&path, support::pdf(&[A4])).unwrap();
    path
}

#[tokio::test]
async fn prints_pdf_to_a_pdf_printer() {
    let dir = TempDir::new("cli");
    let pdf = write_pdf(&dir);
    let fake = FakePrinter::start(Config::default()).await;

    let run = inkdrop_print(&[pdf.to_str().unwrap(), &fake.uri()], &[]).await;

    assert!(run.success, "{}", run.stderr);
    assert_eq!(run.stdout, format!("{}: job 42 accepted (application/pdf), state Pending [none]\n", fake.uri()));
    let jobs = fake.print_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].attr("job-name").as_deref(), Some("letter.pdf"));
    assert_eq!(jobs[0].document, std::fs::read(&pdf).unwrap());
}

#[tokio::test]
async fn converts_to_pwg_raster_when_asked() {
    let dir = TempDir::new("cli");
    let pdf = write_pdf(&dir);
    let fake = FakePrinter::start(Config { raster_resolutions: vec![72], ..Config::default() }).await;

    let run = inkdrop_print(
        &[pdf.to_str().unwrap(), &fake.uri(), "--format", "pwg-raster", "--title", "Quarterly", "-v"],
        &[],
    )
    .await;

    assert!(run.success, "{}", run.stderr);
    assert!(run.stdout.contains("accepted (image/pwg-raster)"), "{}", run.stdout);
    assert!(run.stderr.contains("DEBUG"), "-v logs at debug level: {}", run.stderr);
    let jobs = fake.print_jobs();
    assert_eq!(jobs[0].attr("job-name").as_deref(), Some("Quarterly"));
    assert_eq!(jobs[0].attr("document-format").as_deref(), Some("image/pwg-raster"));
    assert_eq!(&jobs[0].document[..4], b"RaS2");
}

#[tokio::test]
async fn forced_pdf_with_trace_logging_and_a_sparse_reply() {
    let dir = TempDir::new("cli");
    let pdf = write_pdf(&dir);
    let fake = FakePrinter::start(Config {
        formats: vec!["image/pwg-raster"],
        job_id: None,
        job_state: None,
        job_state_reasons: Vec::new(),
        ..Config::default()
    })
    .await;

    let run = inkdrop_print(&[pdf.to_str().unwrap(), &fake.uri(), "--format", "pdf", "-vv"], &[]).await;

    assert!(run.success, "{}", run.stderr);
    assert_eq!(run.stdout, format!("{}: job ? accepted (application/pdf), state unknown\n", fake.uri()));
    assert!(run.stderr.contains("TRACE"), "-vv logs at trace level");
}

#[tokio::test]
async fn reports_errors_and_fails() {
    let dir = TempDir::new("cli");
    let pdf = write_pdf(&dir);
    let pdf = pdf.to_str().unwrap();

    let missing = inkdrop_print(&["/nonexistent/file.pdf", "ipp://127.0.0.1:1/ipp/print"], &[]).await;
    assert!(!missing.success);
    assert!(missing.stderr.starts_with("error: /nonexistent/file.pdf: "), "{}", missing.stderr);

    let bad_uri = inkdrop_print(&[pdf, "not a uri"], &[]).await;
    assert!(!bad_uri.success);
    assert!(bad_uri.stderr.contains("error: invalid printer URI"), "{}", bad_uri.stderr);

    let fake = FakePrinter::start(Config {
        print_status: StatusCode::ClientErrorNotPossible,
        status_message: Some("out of paper"),
        ..Config::default()
    })
    .await;
    let rejected = inkdrop_print(&[pdf, &fake.uri()], &[]).await;
    assert!(!rejected.success);
    assert!(
        rejected.stderr.contains("error: printer rejected the request: ClientErrorNotPossible (out of paper)"),
        "{}",
        rejected.stderr
    );
}

#[tokio::test]
async fn raster_printing_needs_pdfium() {
    let dir = TempDir::new("cli");
    let pdf = write_pdf(&dir);
    let fake = FakePrinter::start(Config::raster_only(72, &["srgb_8"])).await;

    let run = inkdrop_print(&[pdf.to_str().unwrap(), &fake.uri()], &[("PDFIUM_DYNAMIC_LIB_PATH", "/nonexistent")]).await;

    assert!(!run.success);
    assert!(run.stderr.contains("error: PDFium is unavailable"), "{}", run.stderr);
    assert!(fake.print_jobs().is_empty());
}
