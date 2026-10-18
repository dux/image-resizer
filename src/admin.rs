// Admin routes outside the API envelope: cached blob previews (for <img>) and the live logs socket.

use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::io::ReaderStream;

use crate::app::AppState;
use crate::auth::{self, Principal};
use crate::logs::logs;
use crate::resize::handler::text;

fn admin(app: &AppState, headers: &HeaderMap) -> Option<Principal> {
    match auth::resolve(app, headers) {
        Ok(p @ Principal::Admin { .. }) => Some(p),
        _ => None,
    }
}

fn unauthorized() -> Response {
    text(StatusCode::UNAUTHORIZED, "Unauthorized")
}

/// `GET /admin/blob/{id}`: the stored bytes of a cache entry, without triggering a resize.
pub async fn blob(
    State(app): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if admin(&app, &headers).is_none() {
        return unauthorized();
    }
    let store = app.store.clone();
    let Ok(Ok(Some(entry))) = tokio::task::spawn_blocking(move || store.get_by_id(id)).await else {
        return text(StatusCode::NOT_FOUND, "404 page not found");
    };
    let Ok(file) = tokio::fs::File::open(app.store.blob_path(&entry.hash)).await else {
        return text(StatusCode::NOT_FOUND, "404 page not found");
    };
    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&entry.content_type) {
        h.insert(CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&entry.size.to_string()) {
        h.insert(CONTENT_LENGTH, v);
    }
    h.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    crate::resize::handler::harden(h, &entry.content_type);
    response
}

/// `GET /admin/logs`: WebSocket with the last 1000 lines, then live lines.
pub async fn logs_socket(
    State(app): State<AppState>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Some(principal) = admin(&app, &headers) else {
        return unauthorized();
    };
    // browsers send Origin on every WebSocket handshake; refuse other sites riding the cookie
    if principal.via_session() && !auth::same_origin(&headers) {
        return text(StatusCode::FORBIDDEN, "Cross-site request refused");
    }
    match ws {
        Ok(ws) => ws.on_upgrade(stream),
        Err(rejection) => rejection.into_response(),
    }
}

async fn stream(mut socket: WebSocket) {
    let (history, mut rx) = logs().subscribe();
    if !history.is_empty() {
        let joined = history.iter().map(|l| &**l).collect::<Vec<_>>().join("\n");
        if socket.send(Message::Text(joined.into())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            line = rx.recv() => {
                let text = match line {
                    Ok(line) => line.to_string(),
                    Err(RecvError::Lagged(n)) => format!("... skipped {n} lines"),
                    Err(RecvError::Closed) => return,
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
