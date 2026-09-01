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

use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/"]
struct Assets;

pub async fn index(headers: HeaderMap) -> Response {
    serve("index.html", &headers)
}

pub async fn asset(uri: Uri, headers: HeaderMap) -> Response {
    serve(uri.path().trim_start_matches('/'), &headers)
}

/// Serves an embedded file, with an ETag so upgrades are actually seen.
///
/// Without one the browser caches heuristically and keeps serving the old
/// stylesheet after the binary has been replaced, which looks exactly like
/// the new version being broken. `no-cache` here does not mean "do not
/// store", it means "ask me first", so an unchanged file costs a 304 and a
/// changed one arrives immediately.
fn serve(path: &str, headers: &HeaderMap) -> Response {
    let Some(file) = Assets::get(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    let etag = etag_for(&file.metadata.sha256_hash());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| presented.split(',').any(|one| one.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }

    (
        [
            (header::CONTENT_TYPE, mime_for(path).to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

fn etag_for(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(34);
    out.push('"');
    for byte in hash.iter().take(16) {
        out.push_str(&format!("{byte:02x}"));
    }
    out.push('"');
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_the_page_asks_for_is_actually_embedded() {
        // A missing one is a 404 at runtime and a page that half renders.
        for path in ["index.html", "style.css", "app.js"] {
            assert!(Assets::get(path).is_some(), "{path} is not embedded");
        }
    }

    #[test]
    fn the_etag_changes_when_the_file_does() {
        // This is the whole point: same bytes, same tag, so the browser gets
        // a 304. Different bytes, different tag, so an upgrade is seen
        // instead of the old stylesheet being served forever.
        let one = etag_for(&[7u8; 32]);
        let two = etag_for(&[8u8; 32]);
        assert_ne!(one, two);
        assert_eq!(one, etag_for(&[7u8; 32]));
        assert!(one.starts_with('"') && one.ends_with('"'));
    }

    #[test]
    fn a_stylesheet_is_served_as_a_stylesheet() {
        // text/plain here and the browser ignores the whole file.
        assert_eq!(mime_for("style.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime_for("index.html"), "text/html; charset=utf-8");
    }
}
