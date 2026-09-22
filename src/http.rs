use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use futures_util::stream::{Stream, StreamExt};
use serde::Serialize;
use tokio_stream::wrappers::WatchStream;
use tracing::{error, info};

use crate::discovery::{Printer, Registry};
use crate::printing::{self, PrintError};

#[derive(Clone)]
pub struct AppState {
    pub registry: Registry,
}

#[derive(Serialize)]
struct PrinterView {
    id: String,
    name: String,
    address: String,
    model: Option<String>,
    formats: Vec<&'static str>,
}

fn to_views(printers: &HashMap<String, Printer>) -> Vec<PrinterView> {
    let mut views: Vec<PrinterView> = printers
        .values()
        .map(|p| PrinterView {
            id: p.id.clone(),
            name: p.name.clone(),
            address: p.address(),
            model: p.model.clone(),
            formats: p.formats.clone(),
        })
        .collect();
    views.sort_by(|a, b| a.name.cmp(&b.name));
    views
}

pub async fn printers_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let watch_rx = state.registry.subscribe();
    let stream = WatchStream::new(watch_rx).map(|printers| {
        let views = to_views(&printers);
        Ok(Event::default().json_data(views).unwrap_or_else(|_| Event::default().data("[]")))
    });

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

pub async fn print_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    mut multipart: Multipart,
) -> Result<StatusCode, (StatusCode, String)> {
    let printer = {
        let printers = state.registry.borrow();
        printers.get(&id).cloned()
    }
    .ok_or((StatusCode::NOT_FOUND, "printer not found".to_owned()))?;

    let field = multipart
        .next_field()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?
        .ok_or((StatusCode::BAD_REQUEST, "no file provided".to_owned()))?;

    let content_type = field.content_type().unwrap_or_default().to_owned();
    if content_type != "application/pdf" {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "only application/pdf is supported".to_owned(),
        ));
    }

    let job_title = field.file_name().unwrap_or("document.pdf").to_owned();

    let document = field
        .bytes()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;

    info!(
        client_ip = %client_addr.ip(),
        printer_id = %id,
        job_title = %job_title,
        bytes = document.len(),
        "received PDF print job"
    );

    printing::print_pdf(&printer, &job_title, client_addr.ip(), document)
        .await
        .map_err(|err| match err {
            PrintError::UnsupportedFormat => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "printer no longer supports PDF".to_owned(),
            ),
            other => {
                error!(client_ip = %client_addr.ip(), job_title = %job_title, error = %other, "print job failed");
                (StatusCode::BAD_GATEWAY, "printer rejected the job".to_owned())
            }
        })?;

    Ok(StatusCode::NO_CONTENT)
}
