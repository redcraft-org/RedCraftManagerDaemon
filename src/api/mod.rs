//! The REST surface. v1 slice: server supervision, read paths first.
//!
//! Response conventions the whole API keeps:
//! - Player-influenced content (console output) is wrapped in an envelope
//!   with `"untrusted": true` and a label, so no client can forget the
//!   fencing rule (P6). Rendering the literal fence text is the MCP layer's
//!   job; the flag is the API's.
//! - Errors are JSON `{error}` with a meaningful status, not empty bodies.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::servers::{Manager, ServerError};

pub struct AppState {
    pub manager: Arc<Manager>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(crate::web::index))
        .route("/style.css", get(crate::web::asset))
        .route("/app.js", get(crate::web::asset))
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/api/v1/servers", get(list_servers))
        .route("/api/v1/servers/{name}", get(server_status))
        .route("/api/v1/servers/{name}/start", post(start_server))
        .route("/api/v1/servers/{name}/stop", post(stop_server))
        .route("/api/v1/servers/{name}/restart", post(restart_server))
        .route("/api/v1/servers/{name}/command", post(run_command))
        .route("/api/v1/servers/{name}/console", get(console))
        .route("/api/v1/servers/{name}/backup", post(backup))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let status = match &self {
            ServerError::Unknown(_) => StatusCode::NOT_FOUND,
            ServerError::AlreadyRunning(_) | ServerError::NotRunning(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn version(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "name": "rcmd",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": state.started_at,
    }))
}

async fn list_servers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.manager.list().await)
}

async fn server_status(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ServerError> {
    Ok(Json(state.manager.status(&name).await?))
}

async fn start_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ServerError> {
    state.manager.start(&name).await?;
    Ok(Json(json!({ "started": name })))
}

async fn stop_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ServerError> {
    state.manager.stop(&name).await?;
    Ok(Json(json!({ "stopped": name })))
}

#[derive(Deserialize, Default)]
struct RestartBody {
    #[serde(default = "default_true")]
    update_template: bool,
}
fn default_true() -> bool {
    true
}

async fn restart_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: Option<Json<RestartBody>>,
) -> Result<impl IntoResponse, ServerError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    state.manager.restart(&name, body.update_template).await?;
    Ok(Json(json!({ "restarted": name, "update_template": body.update_template })))
}

#[derive(Deserialize)]
struct CommandBody {
    command: String,
}

async fn run_command(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<CommandBody>,
) -> Result<impl IntoResponse, ServerError> {
    state.manager.run_command(&name, &body.command).await?;
    Ok(Json(json!({ "sent": body.command, "to": name })))
}

#[derive(Deserialize)]
struct ConsoleQuery {
    #[serde(default = "default_lines")]
    lines: u32,
}
fn default_lines() -> u32 {
    100
}

async fn console(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ConsoleQuery>,
) -> Result<impl IntoResponse, ServerError> {
    let text = state.manager.console(&name, query.lines.min(5000)).await?;
    Ok(Json(json!({
        "untrusted": true,
        "label": format!("console output from {name}"),
        "content": text,
    })))
}

async fn backup(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ServerError> {
    let report = state.manager.backup(&name).await?;
    Ok(Json(json!({ "backup": report })))
}
