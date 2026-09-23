use std::sync::OnceLock;

use pdfium_render::prelude::*;
use tracing::info;

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
    bind_pdfium_at(pdfium_library_path())
}

fn bind_pdfium_at(library: Option<std::path::PathBuf>) -> Result<Pdfium, PdfiumError> {
    let bindings = match library {
        Some(path) => Pdfium::bind_to_library(path).or_else(|_| Pdfium::bind_to_system_library())?,
        None => Pdfium::bind_to_system_library()?,
    };
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

fn pdfium_library_path() -> Option<std::path::PathBuf> {
    library_path(std::env::var("PDFIUM_DYNAMIC_LIB_PATH").ok(), option_env!("INKDROP_PDFIUM_LIB_DIR"))
}

/// The library in `configured` (`PDFIUM_DYNAMIC_LIB_PATH`) if set, otherwise
/// in `downloaded`, the directory `build.rs` downloaded PDFium to.
fn library_path(configured: Option<String>, downloaded: Option<&str>) -> Option<std::path::PathBuf> {
    let dir = configured.or_else(|| downloaded.map(str::to_owned))?;
    Some(Pdfium::pdfium_platform_library_name_at_path(&dir))
}

/// Render every page of `pdf_bytes` to an RGB8 bitmap at `dpi` dots per inch.
/// Progress is logged per page; the caller's tracing span identifies the job.
pub fn render_pages(pdf_bytes: &[u8], dpi: f32) -> Result<Vec<RenderedPage>, RenderError> {
    let document = pdfium().load_pdf_from_byte_slice(pdf_bytes, None)?;
    let total_pages = document.pages().len();

    document
        .pages()
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let width_pts = page.width().value;
            let height_pts = page.height().value;
            let width_px = ((width_pts / 72.0) * dpi).round().max(1.0) as i32;
            let height_px = ((height_pts / 72.0) * dpi).round().max(1.0) as i32;

            info!(
                page = index + 1,
                total_pages,
                width_px,
                height_px,
                "rendering page"
            );

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, A4};

    #[test]
    fn library_path_prefers_the_configured_directory() {
        let configured = library_path(Some("/opt/pdfium".to_owned()), Some("/downloaded"));
        assert_eq!(configured, Some(Pdfium::pdfium_platform_library_name_at_path("/opt/pdfium")));

        let downloaded = library_path(None, Some("/downloaded"));
        assert_eq!(downloaded, Some(Pdfium::pdfium_platform_library_name_at_path("/downloaded")));

        assert_eq!(library_path(None, None), None);
    }

    #[test]
    fn the_downloaded_library_is_available() {
        assert!(option_env!("INKDROP_PDFIUM_LIB_DIR").is_some(), "build.rs should have downloaded PDFium");
        ensure_available().expect("PDFium should load");
    }

    #[test]
    fn binding_a_missing_library_fails() {
        // Falls back to a system-wide PDFium, which the test machines lack.
        let missing = Some(std::path::PathBuf::from("/nonexistent/libpdfium.so"));
        assert!(bind_pdfium_at(missing).is_err());
        assert!(bind_pdfium_at(None).is_err());
    }

    #[test]
    fn renders_each_page_at_the_requested_resolution() {
        test_support::init_tracing();
        let pdf = test_support::pdf(&[A4, (612.0, 792.0)]);

        let pages = render_pages(&pdf, 72.0).unwrap();

        assert_eq!(pages.len(), 2);
        let a4 = &pages[0];
        assert_eq!((a4.width_px, a4.height_px), (595, 842));
        assert_eq!((a4.width_pts, a4.height_pts), A4);
        assert_eq!(a4.rgb.len(), 595 * 842 * 3);
        assert_eq!((pages[1].width_px, pages[1].height_px), (612, 792));

        // The red square spans 10-30pt from the bottom-left; rows run top down.
        let pixel = |x: usize, y_from_bottom: usize| {
            let offset = ((842 - 1 - y_from_bottom) * 595 + x) * 3;
            &a4.rgb[offset..offset + 3]
        };
        assert_eq!(pixel(20, 20), [255, 0, 0]);
        assert_eq!(pixel(300, 400), [255, 255, 255]);

        let doubled = render_pages(&pdf, 144.0).unwrap();
        assert_eq!((doubled[0].width_px, doubled[0].height_px), (1190, 1684));
    }

    #[test]
    fn tiny_pages_render_at_least_one_pixel() {
        let pages = render_pages(&test_support::pdf(&[(1.0, 1.0)]), 10.0).unwrap();
        assert_eq!((pages[0].width_px, pages[0].height_px), (1, 1));
    }

    #[test]
    fn rejects_bytes_that_are_not_a_pdf() {
        let err = render_pages(b"not a pdf", 72.0).err().expect("should fail");
        assert!(err.to_string().starts_with("PDFium error: "), "{err}");
    }
}
