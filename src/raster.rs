use bytes::{Bytes, BytesMut};

use crate::pdf::RenderedPage;

/// (IPP document-format MIME type, short display label) pairs for the
/// raster formats inkdrop knows how to target. A printer qualifies for
/// display if it advertises (or is confirmed to accept) any of these.
pub const SUPPORTED_FORMATS: &[(&str, &str)] = &[("image/urf", "URF"), ("image/pwg-raster", "PWG-Raster")];

/// Match `formats` (document-format strings, from either an mDNS "pdl" TXT
/// value or a live `document-format-supported` response) against
/// [`SUPPORTED_FORMATS`], returning the display labels of whichever match.
pub fn matching_labels<'a>(formats: impl Iterator<Item = &'a str>) -> Vec<&'static str> {
    let formats: Vec<&str> = formats.collect();
    SUPPORTED_FORMATS
        .iter()
        .filter(|(mime, _)| formats.iter().any(|f| f.eq_ignore_ascii_case(mime)))
        .map(|(_, label)| *label)
        .collect()
}

/// PWG Raster page header size per PWG 5102.4 (identical layout to CUPS's
/// `cups_page_header2_t`), big-endian throughout.
const HEADER_LEN: usize = 1796;
const SYNC_WORD: &[u8; 4] = b"RaS2";

// cups_cspace_t values used on the wire (see CUPS's raster.h).
const CUPS_CSPACE_SW: u32 = 18; // sGray
const CUPS_CSPACE_SRGB: u32 = 19; // sRGB

#[derive(Clone, Copy, Debug)]
pub enum ColorMode {
    Srgb8,
    Sgray8,
}

impl ColorMode {
    fn cups_colorspace(self) -> u32 {
        match self {
            ColorMode::Srgb8 => CUPS_CSPACE_SRGB,
            ColorMode::Sgray8 => CUPS_CSPACE_SW,
        }
    }

    fn components(self) -> u32 {
        match self {
            ColorMode::Srgb8 => 3,
            ColorMode::Sgray8 => 1,
        }
    }
}

/// Encode rendered pages as a PWG Raster (`image/pwg-raster`) byte stream.
pub fn encode(pages: &[RenderedPage], dpi: u32, color: ColorMode) -> Bytes {
    let bytes_per_pixel = color.components();
    let per_page_estimate: usize = pages
        .iter()
        .map(|p| HEADER_LEN + (p.width_px * p.height_px * bytes_per_pixel) as usize)
        .sum();

    let mut out = BytesMut::with_capacity(4 + per_page_estimate);
    out.extend_from_slice(SYNC_WORD);

    let total_pages = pages.len() as u32;

    for page in pages {
        out.extend_from_slice(&page_header(page, dpi, color, total_pages));

        match color {
            ColorMode::Srgb8 => out.extend_from_slice(&page.rgb),
            ColorMode::Sgray8 => {
                out.reserve((page.width_px * page.height_px) as usize);
                for rgb in page.rgb.as_chunks::<3>().0 {
                    let luma = (u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114) / 1000;
                    out.extend_from_slice(&[luma as u8]);
                }
            }
        }
    }

    out.freeze()
}

fn page_header(page: &RenderedPage, dpi: u32, color: ColorMode, total_pages: u32) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    let mut w = Writer { buf: &mut header, pos: 0 };

    // Standard Page Device Dictionary String Values: MediaClass, MediaColor,
    // MediaType, OutputType. Left blank (zeroed) — not needed for a plain
    // single-sided print.
    w.skip(4 * 64);

    w.u32(0); // AdvanceDistance
    w.u32(0); // AdvanceMedia
    w.u32(0); // Collate
    w.u32(0); // CutMedia
    w.u32(0); // Duplex (false: one-sided)
    w.u32(dpi); // HWResolution[0] (cross-feed / horizontal)
    w.u32(dpi); // HWResolution[1] (feed / vertical)
    // ImagingBoundingBox (points): left, bottom, right, top — the whole page.
    w.u32(0);
    w.u32(0);
    w.u32(page.width_pts.round() as u32);
    w.u32(page.height_pts.round() as u32);
    w.u32(0); // InsertSheet
    w.u32(0); // Jog
    w.u32(0); // LeadingEdge
    w.u32(0); // Margins[0]
    w.u32(0); // Margins[1]
    w.u32(0); // ManualFeed
    w.u32(0); // MediaPosition
    w.u32(0); // MediaWeight
    w.u32(0); // MirrorPrint
    w.u32(0); // NegativePrint
    w.u32(1); // NumCopies
    w.u32(0); // Orientation
    w.u32(1); // OutputFaceUp
    w.u32(page.width_pts.round() as u32); // PageSize[0]
    w.u32(page.height_pts.round() as u32); // PageSize[1]
    w.u32(0); // Separations
    w.u32(0); // TraySwitch
    w.u32(0); // Tumble

    w.u32(page.width_px); // cupsWidth
    w.u32(page.height_px); // cupsHeight
    w.u32(0); // cupsMediaType
    w.u32(8); // cupsBitsPerColor
    w.u32(8 * color.components()); // cupsBitsPerPixel
    w.u32(page.width_px * color.components()); // cupsBytesPerLine
    w.u32(0); // cupsColorOrder: chunked
    w.u32(color.cups_colorspace()); // cupsColorSpace
    w.u32(0); // cupsCompression: PWG Raster is always uncompressed
    w.u32(0); // cupsRowCount
    w.u32(0); // cupsRowFeed
    w.u32(0); // cupsRowStep

    w.u32(color.components()); // cupsNumColors
    w.f32(1.0); // cupsBorderlessScalingFactor
    w.f32(page.width_pts); // cupsPageSize[0]
    w.f32(page.height_pts); // cupsPageSize[1]
    w.f32(0.0); // cupsImagingBBox left
    w.f32(0.0); // cupsImagingBBox bottom
    w.f32(page.width_pts); // cupsImagingBBox right
    w.f32(page.height_pts); // cupsImagingBBox top

    w.u32(total_pages); // cupsInteger[0]: TotalPageCount
    for _ in 1..16 {
        w.u32(0); // cupsInteger[1..16]
    }
    for _ in 0..16 {
        w.f32(0.0); // cupsReal[16]
    }
    w.skip(16 * 64); // cupsString[16][64]
    w.skip(64); // cupsMarkerType
    w.skip(64); // cupsRenderingIntent
    w.skip(64); // cupsPageSizeName

    debug_assert_eq!(w.pos, HEADER_LEN);
    header
}

struct Writer<'a> {
    buf: &'a mut [u8; HEADER_LEN],
    pos: usize,
}

impl Writer<'_> {
    fn u32(&mut self, v: u32) {
        self.buf[self.pos..self.pos + 4].copy_from_slice(&v.to_be_bytes());
        self.pos += 4;
    }

    fn f32(&mut self, v: f32) {
        self.buf[self.pos..self.pos + 4].copy_from_slice(&v.to_be_bytes());
        self.pos += 4;
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}
