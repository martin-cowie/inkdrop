//! Printer discovery, the PDF-to-printer pipeline and the web server, shared by
//! the `inkdrop` server and the `inkdrop-print` command-line tool.

pub mod discovery;
pub mod notifications;
pub mod pdf;
pub mod printing;
pub mod raster;
pub mod server;
pub mod status;

// Lets the shared test support module name this crate `inkdrop` in unit
// tests too, as the integration tests must.
#[cfg(test)]
extern crate self as inkdrop;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
