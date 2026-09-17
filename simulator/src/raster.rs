//! Minimal PWG-Raster decoder, just enough to turn what inkdrop sends back
//! into viewable PNGs. Mirrors the header layout inkdrop's own encoder
//! writes (`src/raster.rs` in the main crate, following PWG 5102.4 / CUPS's
//! `cups_page_header2_t`) — not a general-purpose raster reader.

const HEADER_LEN: usize = 1796;
const SYNC_WORD: &[u8; 4] = b"RaS2";

const CUPS_CSPACE_SW: u32 = 18; // sGray
const CUPS_CSPACE_SRGB: u32 = 19; // sRGB

pub struct Page {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("not a PWG-Raster file (missing 'RaS2' sync word)")]
    BadSync,
    #[error("truncated page header")]
    TruncatedHeader,
    #[error("truncated page data: expected {expected} bytes, got {got}")]
    TruncatedData { expected: usize, got: usize },
    #[error("unsupported colorspace code {0}")]
    UnsupportedColorspace(u32),
}

pub fn decode(data: &[u8]) -> Result<Vec<Page>, DecodeError> {
    if data.len() < 4 || &data[0..4] != SYNC_WORD {
        return Err(DecodeError::BadSync);
    }

    let mut pages = Vec::new();
    let mut offset = 4;

    while offset + HEADER_LEN <= data.len() {
        let header = &data[offset..offset + HEADER_LEN];
        offset += HEADER_LEN;

        let width = read_u32(header, 372);
        let height = read_u32(header, 376);
        let bits_per_color = read_u32(header, 384);
        let bytes_per_line = read_u32(header, 392) as usize;
        let colorspace = read_u32(header, 400);

        let data_len = bytes_per_line * height as usize;
        if offset + data_len > data.len() {
            return Err(DecodeError::TruncatedData {
                expected: data_len,
                got: data.len() - offset,
            });
        }
        let pixels = &data[offset..offset + data_len];
        offset += data_len;

        let rgb = to_rgb8(pixels, width as usize, bytes_per_line, colorspace, bits_per_color)?;
        pages.push(Page { width, height, rgb });
    }

    if pages.is_empty() {
        return Err(DecodeError::TruncatedHeader);
    }

    Ok(pages)
}

fn to_rgb8(
    pixels: &[u8],
    width: usize,
    bytes_per_line: usize,
    colorspace: u32,
    bits_per_color: u32,
) -> Result<Vec<u8>, DecodeError> {
    if bits_per_color != 8 {
        return Err(DecodeError::UnsupportedColorspace(colorspace));
    }

    let height = pixels.len() / bytes_per_line.max(1);
    let mut rgb = Vec::with_capacity(width * height * 3);

    match colorspace {
        CUPS_CSPACE_SRGB => {
            for row in pixels.chunks(bytes_per_line) {
                rgb.extend_from_slice(&row[..width * 3]);
            }
        }
        CUPS_CSPACE_SW => {
            for row in pixels.chunks(bytes_per_line) {
                for &gray in &row[..width] {
                    rgb.extend_from_slice(&[gray, gray, gray]);
                }
            }
        }
        other => return Err(DecodeError::UnsupportedColorspace(other)),
    }

    Ok(rgb)
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap())
}
