//! The web UI: static assets compiled into the binary.
//!
//! Embedded rather than served from disk so deployment stays "one binary
//! plus a config file", the same property that makes the daemon easy to
//! install and impossible to half-upgrade.
//!
//! The UI is a client of the public API and nothing more. It holds no
//! privileged path into the service: every button it draws maps to a REST
//! call any token holder could make, which is what keeps the policy layer
//! honest when the in-game and MCP surfaces arrive.

use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/"]
struct Assets;

pub async fn index() -> impl IntoResponse {
    match Assets::get("index.html") {
        Some(file) => Html(file.data.into_owned()).into_response(),
        None => (StatusCode::NOT_FOUND, "web UI was not embedded").into_response(),
    }
}

pub async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            ([(header::CONTENT_TYPE, mime)], file.data.into_owned()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}
