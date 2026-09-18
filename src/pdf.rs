use std::sync::OnceLock;

use pdfium_render::prelude::*;

/// One rasterized PDF page, ready for raster encoding.
pub struct RenderedPage {
    pub width_px: u32,
    pub height_px: u32,
    pub width_pts: f32,
    pub height_pts: f32,
    /// Packed RGB8, row-major, 3 bytes per pixel, no row padding.
    pub rgb: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("PDFium error: {0}")]
    Pdfium(#[from] PdfiumError),
}

static INSTANCE: OnceLock<Pdfium> = OnceLock::new();

fn pdfium() -> &'static Pdfium {
    INSTANCE.get_or_init(|| {
        bind_pdfium().expect("PDFium was verified available at startup via ensure_available(); this should not fail")
    })
}

fn bind_pdfium() -> Result<Pdfium, PdfiumError> {
    let bindings = Pdfium::bind_to_library(pdfium_library_path()).or_else(|_| Pdfium::bind_to_system_library())?;
    Ok(Pdfium::new(bindings))
}

/// Verifies PDFium can be loaded and caches the bound instance, so a missing
/// or blocked library (e.g. macOS Gatekeeper quarantining `libpdfium.dylib`)
/// is reported clearly at startup instead of surfacing mid-print.
pub fn ensure_available() -> Result<(), PdfiumError> {
    let instance = bind_pdfium()?;
    let _ = INSTANCE.set(instance);
    Ok(())
}

fn pdfium_library_path() -> std::path::PathBuf {
    let dir = std::env::var("PDFIUM_DYNAMIC_LIB_PATH").unwrap_or_else(|_| "native/pdfium/lib".to_owned());
    Pdfium::pdfium_platform_library_name_at_path(&dir)
}

/// Render every page of `pdf_bytes` to an RGB8 bitmap at `dpi` dots per inch.
pub fn render_pages(pdf_bytes: &[u8], dpi: f32) -> Result<Vec<RenderedPage>, RenderError> {
    let document = pdfium().load_pdf_from_byte_slice(pdf_bytes, None)?;

    document
        .pages()
        .iter()
        .map(|page| {
            let width_pts = page.width().value;
            let height_pts = page.height().value;
            let width_px = ((width_pts / 72.0) * dpi).round().max(1.0) as i32;
            let height_px = ((height_pts / 72.0) * dpi).round().max(1.0) as i32;

            let bitmap = page.render_with_config(
                &PdfRenderConfig::new()
                    .set_fixed_size(width_px, height_px)
                    .use_print_quality(true)
                    .clear_before_rendering(true)
                    .set_clear_color(PdfColor::WHITE),
            )?;

            let rgba = bitmap.as_rgba_bytes();
            let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
            for pixel in rgba.as_chunks::<4>().0 {
                rgb.extend_from_slice(&pixel[..3]);
            }

            Ok(RenderedPage {
                width_px: width_px as u32,
                height_px: height_px as u32,
                width_pts,
                height_pts,
                rgb,
            })
        })
        .collect()
}
