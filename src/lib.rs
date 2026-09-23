//! Printer discovery and the PDF-to-printer pipeline, shared by the `inkdrop`
//! web server and the `inkdrop-print` command-line tool.

pub mod discovery;
pub mod pdf;
pub mod printing;
pub mod raster;
