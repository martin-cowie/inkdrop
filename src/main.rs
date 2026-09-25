//! The inkdrop web server: discovers printers, serves the frontend from
//! `frontend/dist` and prints PDFs uploaded to it. Listens on `PORT` (default
//! 8080) and also lists the printers named in `INKDROP_PRINTERS`.

use std::collections::HashMap;
use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use inkdrop::server::{self, AppState};
use inkdrop::{discovery, pdf};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("inkdrop=info")))
        .init();

    if let Err(err) = pdf::ensure_available() {
        tracing::error!(%err, "PDFium is unavailable; Cannot start");
        std::process::exit(1);
    }

    let (registry, _) = watch::channel(HashMap::new());
    discovery::spawn(registry.clone());

    let configured = server::configured_printers(&std::env::var("INKDROP_PRINTERS").unwrap_or_default());
    discovery::spawn_configured(configured, registry.clone());

    let app = server::router(AppState { registry }, "frontend/dist");

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
        .await
        .expect("failed to bind listener");
    // The bound address, so `PORT=0` reports the port the OS chose.
    let addr = listener.local_addr().expect("listener has a local address");
    tracing::info!(%addr, "inkdrop listening");

    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());
    // Exit promptly rather than draining connections: browsers hold the
    // printer event stream open indefinitely.
    tokio::select! {
        result = server => result.expect("server error"),
        () = shutdown_signal() => tracing::info!("inkdrop stopping"),
    }
}

/// Ctrl-C, or SIGTERM (e.g. `docker stop`) on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
