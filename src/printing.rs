use std::io::Cursor;
use std::net::IpAddr;

use bytes::Bytes;
use ipp::parser::IppParseError;
use ipp::prelude::*;
use tracing::info;

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
}

/// How a PDF will actually reach the printer.
enum PrintPlan {
    /// The printer accepts `application/pdf` directly; send the bytes as-is.
    DirectPdf,
    /// The printer doesn't take PDF, but does take `image/pwg-raster`; the
    /// PDF is rasterized locally first.
    PwgRaster { dpi: u32, color: ColorMode },
}

/// Confirm how the printer wants the document, converting if necessary, then
/// submit it as a Print-Job.
pub async fn print_pdf(printer: &Printer, job_title: &str, client_ip: IpAddr, document: Bytes) -> Result<(), PrintError> {
    let uri: Uri = printer.ipp_uri().parse()?;
    let client = AsyncIppClient::new(uri.clone());

    let plan = plan_print(&client, uri.clone()).await?;

    let (payload, document_format) = match plan {
        PrintPlan::DirectPdf => {
            info!(%client_ip, %job_title, "printer accepts PDF directly; sending document as-is");
            (IppPayload::new(Cursor::new(document)), "application/pdf")
        }
        PrintPlan::PwgRaster { dpi, color } => {
            info!(%client_ip, %job_title, dpi, ?color, "printer requires PWG-Raster; rendering PDF page by page");
            let pages = crate::pdf::render_pages(&document, dpi as f32, job_title, client_ip)?;
            let raster = crate::raster::encode(&pages, dpi, color);
            info!(%client_ip, %job_title, page_count = pages.len(), "encoded raster document");
            (IppPayload::new(Cursor::new(raster)), "image/pwg-raster")
        }
    };

    let operation = IppOperationBuilder::print_job(uri, payload)
        .user_name("inkdrop")
        .job_title(job_title)
        .document_format(document_format)
        .build()?;

    let response = client.send(operation).await?;
    if !response.header().status_code().is_success() {
        return Err(PrintError::Ipp(IppError::StatusError(response.header().status_code())));
    }

    info!(%client_ip, %job_title, "print job accepted by printer");
    Ok(())
}

/// Query the printer directly for the raster formats it advertises, for use
/// when mDNS TXT records don't mention "pdl" at all. Any failure
/// (unreachable, malformed response, etc.) is treated as "none" rather than
/// propagated, since this is a best-effort discovery-time probe.
pub async fn probe_raster_formats(uri: &Uri) -> Vec<&'static str> {
    let client = AsyncIppClient::new(uri.clone());
    match document_formats(&client, uri.clone()).await {
        Ok(formats) => crate::raster::matching_labels(formats.iter().map(String::as_str)),
        Err(err) => {
            tracing::debug!(%uri, error = %err, "failed to probe printer capabilities");
            Vec::new()
        }
    }
}

async fn document_formats(client: &AsyncIppClient, uri: Uri) -> Result<Vec<String>, PrintError> {
    let operation = IppOperationBuilder::get_printer_attributes(uri)
        .attribute(IppAttribute::DOCUMENT_FORMAT_SUPPORTED)
        .build()?;

    let response = client.send(operation).await?;

    let formats = response
        .attributes()
        .first_of(DelimiterTag::PrinterAttributes)
        .and_then(|g| g.get(IppAttribute::DOCUMENT_FORMAT_SUPPORTED))
        .map(|attr| flatten(attr.value()).iter().map(|v| v.to_string()).collect())
        .unwrap_or_default();

    Ok(formats)
}

async fn plan_print(client: &AsyncIppClient, uri: Uri) -> Result<PrintPlan, PrintError> {
    let operation = IppOperationBuilder::get_printer_attributes(uri)
        .attribute(IppAttribute::DOCUMENT_FORMAT_SUPPORTED)
        .attribute("pwg-raster-document-resolution-supported")
        .attribute("pwg-raster-document-type-supported")
        .build()?;

    let response = client.send(operation).await?;
    let group = response.attributes().first_of(DelimiterTag::PrinterAttributes);

    let format_supported = |format: &str| {
        group
            .and_then(|g| g.get(IppAttribute::DOCUMENT_FORMAT_SUPPORTED))
            .is_some_and(|attr| flatten(attr.value()).iter().any(|v| value_is(v, format)))
    };

    if format_supported("application/pdf") {
        return Ok(PrintPlan::DirectPdf);
    }

    if !format_supported("image/pwg-raster") {
        return Err(PrintError::UnsupportedFormat);
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
