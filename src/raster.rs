//! The document formats inkdrop supports, and a PWG Raster (PWG 5102.4)
//! encoder for rendered pages.

use std::borrow::Cow;

use bytes::{BufMut, Bytes, BytesMut};

use crate::pdf::RenderedPage;

/// (IPP document-format MIME type, short display label) pairs for the
/// formats inkdrop knows how to target: PDF sent as-is, or a raster format.
/// A printer qualifies for display if it advertises (or is confirmed to
/// accept) any of these.
pub const SUPPORTED_FORMATS: &[(&str, &str)] = &[
    ("application/pdf", "PDF"),
    ("image/urf", "URF"),
    ("image/pwg-raster", "PWG-Raster"),
];

/// Match `formats` (document-format strings, from either an mDNS "pdl" TXT
/// value or a live `document-format-supported` response) against
/// [`SUPPORTED_FORMATS`], ignoring case, returning the display labels of
/// whichever match in the order of [`SUPPORTED_FORMATS`].
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
const CUPS_CSPACE_SRGB: u32 = 19;

/// A PWG Raster colour mode, as named in `pwg-raster-document-type-supported`.
#[derive(Clone, Copy, Debug)]
pub enum ColorMode {
    /// `srgb_8`: 8-bit sRGB.
    Srgb8,
    /// `sgray_8`: 8-bit greyscale.
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

/// Encode `pages` as a PWG Raster (`image/pwg-raster`) document at `dpi` dots
/// per inch in colour mode `color`, returning its bytes. Pages must have
/// been rendered at `dpi`.
pub fn encode(pages: &[RenderedPage], dpi: u32, color: ColorMode) -> Bytes {
    let mut out = BytesMut::new();
    out.extend_from_slice(SYNC_WORD);

    let total_pages = pages.len() as u32;

    for (index, page) in pages.iter().enumerate() {
        tracing::debug!(
            page = index + 1,
            width_px = page.width_px,
            height_px = page.height_px,
            width_pts = page.width_pts,
            height_pts = page.height_pts,
            dpi,
            ?color,
            page_size_name = page_size_name(page),
            "writing PWG-Raster page header"
        );
        out.extend_from_slice(&page_header(page, dpi, color, total_pages));

        let pixels: Cow<[u8]> = match color {
            ColorMode::Srgb8 => Cow::Borrowed(&page.rgb),
            ColorMode::Sgray8 => Cow::Owned(
                page.rgb
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|rgb| ((u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114) / 1000) as u8)
                    .collect(),
            ),
        };
        compress_page(&mut out, &pixels, page.width_px as usize, color.components() as usize);
    }

    out.freeze()
}

/// Compress page pixels as PWG 5102.4 section 4.3 requires: each line starts
/// with a count of how many following lines are identical to it (0-255),
/// then the line's pixels as PackBits-style runs of up to 128 pixels.
fn compress_page(out: &mut BytesMut, pixels: &[u8], width: usize, bytes_per_pixel: usize) {
    let mut lines = pixels.chunks_exact(width * bytes_per_pixel).peekable();
    while let Some(line) = lines.next() {
        let mut repeat = 0u8;
        while repeat < u8::MAX && lines.next_if(|next| *next == line).is_some() {
            repeat += 1;
        }
        out.put_u8(repeat);
        compress_line(out, line, bytes_per_pixel);
    }
}

/// A control byte of `n` (0-127) means the next pixel is repeated `n + 1`
/// times; `257 - n` (129-255) means `n` (2-128) literal pixels follow.
fn compress_line(out: &mut BytesMut, line: &[u8], bytes_per_pixel: usize) {
    const MAX_RUN: usize = 128;

    let count = line.len() / bytes_per_pixel;
    let pixel = |i: usize| &line[i * bytes_per_pixel..(i + 1) * bytes_per_pixel];
    let run_starts_at = |i: usize| i + 1 < count && pixel(i) == pixel(i + 1);

    let mut i = 0;
    while i < count {
        let mut run = 1;
        while i + run < count && run < MAX_RUN && pixel(i + run) == pixel(i) {
            run += 1;
        }

        if run > 1 {
            out.put_u8((run - 1) as u8);
            out.extend_from_slice(pixel(i));
            i += run;
            continue;
        }

        let start = i;
        i += 1;
        while i < count && i - start < MAX_RUN && !run_starts_at(i) {
            i += 1;
        }
        let literal = i - start;
        if literal == 1 {
            out.put_u8(0);
        } else {
            out.put_u8((257 - literal) as u8);
        }
        out.extend_from_slice(&line[start * bytes_per_pixel..i * bytes_per_pixel]);
    }
}

/// The PWG self-describing media name for common paper sizes, which some
/// printers use to select a tray; empty (unspecified) for anything else.
fn page_size_name(page: &RenderedPage) -> &'static str {
    const SIZES: &[(f32, f32, &str)] = &[
        (595.0, 842.0, "iso_a4_210x297mm"),
        (420.0, 595.0, "iso_a5_148x210mm"),
        (842.0, 1191.0, "iso_a3_297x420mm"),
        (612.0, 792.0, "na_letter_8.5x11in"),
        (612.0, 1008.0, "na_legal_8.5x14in"),
    ];
    let matches = |a: f32, b: f32| (a - b).abs() <= 2.0;
    SIZES
        .iter()
        .find(|(w, h, _)| matches(page.width_pts, *w) && matches(page.height_pts, *h))
        .map_or("", |(_, _, name)| name)
}

/// Fields not set here are reserved by PWG 5102.4 and must be zero.
fn page_header(page: &RenderedPage, dpi: u32, color: ColorMode, total_pages: u32) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    let mut w = Writer { buf: &mut header, pos: 0 };

    w.str(b"PwgRaster"); // MediaClass: always "PwgRaster"
    w.skip(64); // MediaColor
    w.skip(64); // MediaType
    w.skip(64); // PrintContentOptimize (CUPS OutputType)

    w.skip(4 * 4); // Reserved (AdvanceDistance, AdvanceMedia, Collate), CutMedia
    w.u32(0); // Duplex (false: one-sided)
    w.u32(dpi); // HWResolution[0] (cross-feed / horizontal)
    w.u32(dpi); // HWResolution[1] (feed / vertical)
    w.skip(4 * 4); // Reserved (ImagingBoundingBox)
    w.skip(4 * 4); // InsertSheet, Jog, LeadingEdge, Reserved (Margins[0])
    w.skip(4 * 2); // Reserved (Margins[1], ManualFeed)
    w.u32(0); // MediaPosition: auto
    w.u32(0); // MediaWeight: unspecified
    w.skip(4 * 2); // Reserved (MirrorPrint, NegativePrint)
    w.u32(1); // NumCopies
    w.u32(0); // Orientation: portrait
    w.skip(4); // Reserved (OutputFaceUp)
    w.u32(page.width_pts.round() as u32); // PageSize[0], points
    w.u32(page.height_pts.round() as u32); // PageSize[1], points
    w.skip(4 * 2); // Reserved (Separations, TraySwitch)
    w.u32(0); // Tumble

    w.u32(page.width_px); // Width
    w.u32(page.height_px); // Height
    w.skip(4); // Reserved (cupsMediaType)
    w.u32(8); // BitsPerColor
    w.u32(8 * color.components()); // BitsPerPixel
    w.u32(page.width_px * color.components()); // BytesPerLine (uncompressed)
    w.u32(0); // ColorOrder: chunky
    w.u32(color.cups_colorspace()); // ColorSpace
    w.skip(4 * 4); // Reserved (cupsCompression, cupsRowCount, cupsRowFeed, cupsRowStep)
    w.u32(color.components()); // NumColors
    w.skip(4 * 7); // Reserved (cupsBorderlessScalingFactor, cupsPageSize, cupsImagingBBox)

    w.u32(total_pages); // TotalPageCount
    w.u32(1); // CrossFeedTransform
    w.u32(1); // FeedTransform
    w.u32(0); // ImageBoxLeft
    w.u32(0); // ImageBoxTop
    w.u32(page.width_px); // ImageBoxRight
    w.u32(page.height_px); // ImageBoxBottom
    w.u32(0); // AlternatePrimary
    w.u32(0); // PrintQuality: default
    w.skip(4 * 5); // Reserved
    w.u32(0); // VendorIdentifier
    w.u32(0); // VendorLength
    w.skip(1088); // VendorData
    w.skip(64); // Reserved (cupsMarkerType)
    w.skip(64); // RenderingIntent
    w.str(page_size_name(page).as_bytes()); // PageSizeName

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

    /// A NUL-terminated string in a 64-byte field.
    fn str(&mut self, s: &[u8]) {
        debug_assert!(s.len() < 64);
        self.buf[self.pos..self.pos + s.len()].copy_from_slice(s);
        self.pos += 64;
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode one compressed line the way PWG raster readers (e.g.
    /// ippdoclint's `read_raster_image`) do.
    fn decompress_line(data: &mut &[u8], width: usize, bpp: usize) -> Vec<u8> {
        let mut line = Vec::new();
        while line.len() < width * bpp {
            let ch = data[0] as usize;
            *data = &data[1..];
            let (count, bytes) = if ch & 0x80 != 0 { (257 - ch, (257 - ch) * bpp) } else { (ch + 1, bpp) };
            assert!(ch != 0x80, "encoder never emits clear-to-end-of-line");
            assert!(line.len() + count * bpp <= width * bpp, "run overflows line");
            let fragment = &data[..bytes];
            *data = &data[bytes..];
            if bytes == bpp {
                for _ in 0..count {
                    line.extend_from_slice(fragment);
                }
            } else {
                line.extend_from_slice(fragment);
            }
        }
        line
    }

    fn roundtrip(line: &[u8], bpp: usize) {
        let mut out = BytesMut::new();
        compress_line(&mut out, line, bpp);
        let mut data = &out[..];
        assert_eq!(decompress_line(&mut data, line.len() / bpp, bpp), line);
        assert!(data.is_empty(), "trailing bytes after line");
    }

    #[test]
    fn compress_line_roundtrips() {
        roundtrip(&[7], 1);
        roundtrip(&[1, 2], 1);
        roundtrip(&[1, 1], 1);
        roundtrip(&[1, 2, 2, 3, 4, 5, 5, 5, 6], 1);
        roundtrip(&[255; 1000], 1);
        roundtrip(&(0..=255).cycle().take(1000).collect::<Vec<u8>>(), 1);
        roundtrip(&[1, 2, 3, 1, 2, 3, 9, 9, 9, 4, 5, 6], 3);
        let mixed: Vec<u8> = (0..3000u32).map(|i| if i % 700 < 300 { 255 } else { (i * 7 % 251) as u8 }).collect();
        roundtrip(&mixed, 3);
    }

    #[test]
    fn compress_page_collapses_identical_lines() {
        let mut out = BytesMut::new();
        compress_page(&mut out, &[255u8; 4 * 300], 4, 1);
        // 300 identical lines: one line repeated 256 times, then 44.
        assert_eq!(&out[..], &[255, 3, 255, 43, 3, 255]);
    }

    #[test]
    fn header_is_pwg_raster() {
        let page = RenderedPage { width_px: 1240, height_px: 1754, width_pts: 595.0, height_pts: 842.0, rgb: Vec::new() };
        let header = page_header(&page, 150, ColorMode::Srgb8, 1);
        assert_eq!(&header[..10], b"PwgRaster\0");
        assert_eq!(&header[1732..1748], b"iso_a4_210x297mm");
    }

    fn be_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn page(width_px: u32, height_px: u32, rgb: Vec<u8>) -> RenderedPage {
        RenderedPage { width_px, height_px, width_pts: 612.0, height_pts: 792.0, rgb }
    }

    #[test]
    fn matching_labels_follow_supported_order_ignoring_case() {
        let labels = matching_labels(["IMAGE/PWG-RASTER", "text/plain", "application/pdf", "image/urf"].into_iter());
        assert_eq!(labels, ["PDF", "URF", "PWG-Raster"]);
        assert!(matching_labels(["image/jpeg"].into_iter()).is_empty());
        assert!(matching_labels(std::iter::empty()).is_empty());
    }

    #[test]
    fn header_fields_sit_at_their_pwg_offsets() {
        let page = RenderedPage { width_px: 1700, height_px: 2200, width_pts: 612.4, height_pts: 791.6, rgb: Vec::new() };

        let header = page_header(&page, 200, ColorMode::Srgb8, 3);
        assert_eq!(be_u32(&header, 276), 200, "HWResolution[0]");
        assert_eq!(be_u32(&header, 280), 200, "HWResolution[1]");
        assert_eq!(be_u32(&header, 340), 1, "NumCopies");
        assert_eq!(be_u32(&header, 352), 612, "PageSize[0], rounded");
        assert_eq!(be_u32(&header, 356), 792, "PageSize[1], rounded");
        assert_eq!(be_u32(&header, 372), 1700, "Width");
        assert_eq!(be_u32(&header, 376), 2200, "Height");
        assert_eq!(be_u32(&header, 384), 8, "BitsPerColor");
        assert_eq!(be_u32(&header, 388), 24, "BitsPerPixel");
        assert_eq!(be_u32(&header, 392), 1700 * 3, "BytesPerLine");
        assert_eq!(be_u32(&header, 400), CUPS_CSPACE_SRGB, "ColorSpace");
        assert_eq!(be_u32(&header, 420), 3, "NumColors");
        assert_eq!(be_u32(&header, 452), 3, "TotalPageCount");
        assert_eq!(be_u32(&header, 456), 1, "CrossFeedTransform");
        assert_eq!(be_u32(&header, 460), 1, "FeedTransform");
        assert_eq!(be_u32(&header, 472), 1700, "ImageBoxRight");
        assert_eq!(be_u32(&header, 476), 2200, "ImageBoxBottom");
        assert_eq!(&header[1732..1751], b"na_letter_8.5x11in\0");

        let gray = page_header(&page, 300, ColorMode::Sgray8, 1);
        assert_eq!(be_u32(&gray, 388), 8, "BitsPerPixel");
        assert_eq!(be_u32(&gray, 392), 1700, "BytesPerLine");
        assert_eq!(be_u32(&gray, 400), CUPS_CSPACE_SW, "ColorSpace");
        assert_eq!(be_u32(&gray, 420), 1, "NumColors");
    }

    #[test]
    fn page_size_names_allow_a_little_slack() {
        let named = |width_pts, height_pts| {
            page_size_name(&RenderedPage { width_px: 1, height_px: 1, width_pts, height_pts, rgb: Vec::new() })
        };
        assert_eq!(named(595.0, 842.0), "iso_a4_210x297mm");
        assert_eq!(named(596.9, 840.1), "iso_a4_210x297mm");
        assert_eq!(named(420.0, 595.0), "iso_a5_148x210mm");
        assert_eq!(named(842.0, 1191.0), "iso_a3_297x420mm");
        assert_eq!(named(612.0, 792.0), "na_letter_8.5x11in");
        assert_eq!(named(612.0, 1008.0), "na_legal_8.5x14in");
        assert_eq!(named(842.0, 595.0), "", "landscape A4 isn't a PWG name");
        assert_eq!(named(598.0, 842.0), "", "beyond the 2pt slack");
        assert_eq!(named(100.0, 100.0), "");
    }

    /// Split an encoded document back into (header, decoded pixels) pages.
    fn decode(mut data: &[u8], bpp: usize) -> Vec<([u8; HEADER_LEN], Vec<u8>)> {
        assert_eq!(&data[..4], SYNC_WORD);
        data = &data[4..];
        let mut pages = Vec::new();
        while !data.is_empty() {
            let header: [u8; HEADER_LEN] = data[..HEADER_LEN].try_into().unwrap();
            data = &data[HEADER_LEN..];
            let (width, height) = (be_u32(&header, 372) as usize, be_u32(&header, 376) as usize);
            let mut pixels = Vec::new();
            let mut lines = 0;
            while lines < height {
                let repeat = data[0] as usize;
                data = &data[1..];
                let line = decompress_line(&mut data, width, bpp);
                for _ in 0..=repeat {
                    pixels.extend_from_slice(&line);
                }
                lines += repeat + 1;
            }
            assert_eq!(lines, height, "line repeats overran the page");
            pages.push((header, pixels));
        }
        pages
    }

    #[test]
    fn encode_writes_every_page_in_color() {
        let red_white = [255, 0, 0, 255, 255, 255].repeat(2);
        let pages = [page(2, 2, red_white.clone()), page(1, 1, vec![1, 2, 3])];

        let encoded = encode(&pages, 300, ColorMode::Srgb8);

        let decoded = decode(&encoded, 3);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].1, red_white);
        assert_eq!(decoded[1].1, [1, 2, 3]);
        assert_eq!(be_u32(&decoded[0].0, 452), 2, "TotalPageCount");
        assert_eq!(be_u32(&decoded[1].0, 452), 2, "TotalPageCount");
    }

    #[test]
    fn encode_converts_to_luma_in_gray() {
        let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255].to_vec();

        let encoded = encode(&[page(4, 1, pixels)], 150, ColorMode::Sgray8);

        let decoded = decode(&encoded, 1);
        assert_eq!(decoded[0].1, [76, 149, 29, 255]);
        assert_eq!(be_u32(&decoded[0].0, 276), 150);
    }

    #[test]
    fn encode_with_no_pages_is_just_the_sync_word() {
        assert_eq!(&encode(&[], 300, ColorMode::Srgb8)[..], SYNC_WORD);
    }

    #[test]
    fn compress_line_splits_long_runs_and_literals() {
        // 300 identical pixels: runs of 128, 128 and 44.
        let mut out = BytesMut::new();
        compress_line(&mut out, &[9; 300], 1);
        assert_eq!(&out[..], &[127, 9, 127, 9, 43, 9]);

        // 130 distinct pixels: a literal of 128, then a literal of 2.
        let distinct: Vec<u8> = (0..130).collect();
        let mut out = BytesMut::new();
        compress_line(&mut out, &distinct, 1);
        assert_eq!(out[0], 129);
        assert_eq!(&out[1..129], &distinct[..128]);
        assert_eq!(out[129], 255);
        assert_eq!(&out[130..], &distinct[128..]);
    }
}
