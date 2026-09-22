mod discovery;
mod http;
mod pdf;
mod printing;
mod raster;

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

use crate::http::AppState;

const MAX_UPLOAD_BYTES: usize = 200 * 1024 * 1024;

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

    let state = AppState { registry };

    let serve_frontend = ServeDir::new("frontend/dist");

    let app = Router::new()
        .route("/api/printers", get(http::printers_stream))
        .route(
            "/api/print/{id}",
            post(http::print_handler).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .with_state(state)
        .fallback_service(serve_frontend);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let listener = TcpListener::bind(addr).await.expect("failed to bind listener");
    tracing::info!(%addr, "inkdrop listening");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("server error");
}
