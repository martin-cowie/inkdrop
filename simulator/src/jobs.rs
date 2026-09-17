use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::raster;

#[derive(Clone)]
pub struct JobStore {
    dir: Arc<PathBuf>,
}

pub struct SavedJob {
    pub path: PathBuf,
    pub preview_pages: usize,
    /// Set if `document_format` claimed a format this simulator can decode
    /// (currently just PWG-Raster) but decoding it failed — a strong signal
    /// the sender's encoder produced a malformed stream.
    pub preview_error: Option<String>,
}

impl JobStore {
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir: Arc::new(dir) })
    }

    pub fn save(
        &self,
        job_id: i32,
        document_format: &str,
        job_name: Option<&str>,
        data: &[u8],
    ) -> std::io::Result<SavedJob> {
        let ext = extension_for(document_format);
        let stem = format!("{:04}-{}", job_id, slug(job_name.unwrap_or("job")));
        let path = self.dir.join(format!("{stem}.{ext}"));
        fs::write(&path, data)?;

        let mut preview_pages = 0;
        let mut preview_error = None;

        if document_format.eq_ignore_ascii_case("image/pwg-raster") {
            match raster::decode(data) {
                Ok(pages) => {
                    for (i, page) in pages.iter().enumerate() {
                        let png_path = self.dir.join(format!("{stem}-page{i}.png"));
                        if let Err(err) = image::save_buffer(
                            &png_path,
                            &page.rgb,
                            page.width,
                            page.height,
                            image::ColorType::Rgb8,
                        ) {
                            preview_error = Some(format!("failed to write preview PNG: {err}"));
                            break;
                        }
                    }
                    preview_pages = pages.len();
                }
                Err(err) => preview_error = Some(err.to_string()),
            }
        }

        Ok(SavedJob {
            path,
            preview_pages,
            preview_error,
        })
    }
}

fn extension_for(document_format: &str) -> &'static str {
    match document_format.to_ascii_lowercase().as_str() {
        "image/pwg-raster" => "pwg",
        "image/urf" => "urf",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// A filesystem-safe, human-readable stem from a job name.
fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "job".to_owned()
    } else {
        trimmed.chars().take(40).collect()
    }
}
