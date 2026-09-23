//! Printer discovery, the PDF-to-printer pipeline and the web server, shared by
//! the `inkdrop` server and the `inkdrop-print` command-line tool.

pub mod discovery;
pub mod pdf;
pub mod printing;
pub mod raster;
pub mod server;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
