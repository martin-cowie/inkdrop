//! Print a PDF to a given IPP printer from the command line, using the same
//! conversion and submission code as the inkdrop server. For testing the
//! "print this PDF to that printer" step without the web UI or mDNS.

use std::path::PathBuf;
use std::process::ExitCode;

use bytes::Bytes;
use clap::{ArgAction, Parser, ValueEnum};
use inkdrop::printing::{self, DocumentFormat, PrintError, PrintPlan};
use ipp::prelude::*;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(about = "Print a PDF to an IPP printer, converting it to a format the printer accepts")]
struct Args {
    /// PDF file to print.
    pdf: PathBuf,

    /// Printer URI, e.g. ipp://192.168.1.20:631/ipp/print
    printer: String,

    /// Document format to send. `auto` uses what the printer advertises,
    /// preferring PDF; the others are sent even if the printer doesn't
    /// advertise them.
    #[arg(long, value_enum, default_value_t = Format::Auto)]
    format: Format,

    /// Job title (defaults to the PDF's file name).
    #[arg(long)]
    title: Option<String>,

    /// More diagnostics: -v logs the plan, per-page raster headers and IPP
    /// operation/job attributes; -vv also dumps every printer attribute.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Auto,
    Pdf,
    PwgRaster,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let level = match args.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(format!("inkdrop={level},inkdrop_print={level}"))),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let document = Bytes::from(tokio::fs::read(&args.pdf).await.map_err(|err| format!("{}: {err}", args.pdf.display()))?);
    let job_title = args
        .title
        .unwrap_or_else(|| args.pdf.file_name().map_or_else(|| "document.pdf".to_owned(), |n| n.to_string_lossy().into_owned()));

    let uri: Uri = args.printer.parse().map_err(PrintError::from)?;
    let client = AsyncIppClient::new(uri.clone());

    let forced = match args.format {
        Format::Auto => None,
        Format::Pdf => Some(DocumentFormat::Pdf),
        Format::PwgRaster => Some(DocumentFormat::PwgRaster),
    };

    let plan = printing::plan_print(&client, uri.clone(), forced).await?;
    tracing::info!(?plan, "print plan");

    if matches!(plan, PrintPlan::PwgRaster { .. }) {
        inkdrop::pdf::ensure_available().map_err(|err| format!("PDFium is unavailable: {err}"))?;
    }

    let (payload, format) = printing::render(&plan, document)?;
    let submitted = printing::submit(&client, uri, payload, format, &job_title).await?;

    println!(
        "{}: job {} accepted ({}), state {}{}",
        args.printer,
        submitted.job_id.map_or_else(|| "?".to_owned(), |id| id.to_string()),
        format.mime_type(),
        submitted.job_state.as_deref().unwrap_or("unknown"),
        if submitted.job_state_reasons.is_empty() {
            String::new()
        } else {
            format!(" [{}]", submitted.job_state_reasons.join(", "))
        },
    );
    Ok(())
}
