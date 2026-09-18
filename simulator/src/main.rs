mod ipp_server;
mod jobs;
mod raster;

use std::net::SocketAddr;
use std::sync::atomic::AtomicI32;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::post;
use axum::Router;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::ipp_server::AppState;
use crate::jobs::JobStore;

const RESOURCE_PATH: &str = "ipp/print";
const INSTANCE_NAME: &str = "Inkdrop Simulated Printer";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("printer_sim=info")))
        .init();

    // Default to an OS-assigned ephemeral port so a leftover instance from a
    // previous run can't block a new one; set PORT for a fixed, predictable
    // one (handy for manual curl/ipptool testing).
    let requested_port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(0);

    let jobs_dir = std::env::var("JOBS_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| "jobs".into());
    let jobs = JobStore::new(jobs_dir).expect("failed to create jobs directory");

    let state = AppState {
        jobs,
        next_job_id: Arc::new(AtomicI32::new(1)),
    };

    // Uncompressed raster pages are large (an US-Letter page at 300dpi RGB8
    // is ~25MB); axum's 2MB default body limit would reject real jobs.
    const MAX_JOB_BYTES: usize = 500 * 1024 * 1024;

    let app = Router::new()
        .route(
            &format!("/{RESOURCE_PATH}"),
            post(ipp_server::handle).layer(DefaultBodyLimit::max(MAX_JOB_BYTES)),
        )
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], requested_port));
    let listener = TcpListener::bind(addr).await.expect("failed to bind listener");
    let port = listener.local_addr().expect("bound listener has a local address").port();
    tracing::info!(%addr, port, resource_path = RESOURCE_PATH, "printer-sim listening");

    let mdns = ServiceDaemon::new().expect("failed to create mDNS daemon");
    let service_info = ServiceInfo::new(
        "_ipp._tcp.local.",
        INSTANCE_NAME,
        "inkdrop-sim.local.",
        "",
        port,
        &[
            ("rp", RESOURCE_PATH),
            ("ty", "Inkdrop Simulated Printer"),
            ("pdl", "image/pwg-raster,image/urf"),
            ("product", "(Inkdrop Simulator)"),
            ("txtvers", "1"),
            ("qtotal", "1"),
        ][..],
    )
    .expect("valid mDNS service info")
    .enable_addr_auto();

    let fullname = service_info.get_fullname().to_string();
    mdns.register(service_info).expect("failed to register mDNS service");
    tracing::info!(%fullname, port, "advertising via mDNS");

    let serve = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());
    tokio::select! {
        result = serve => {
            if let Err(err) = result {
                tracing::error!(%err, "server error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down, unregistering mDNS service");
            if let Ok(receiver) = mdns.unregister(&fullname) {
                let _ = receiver.recv();
            }
        }
    }
}
