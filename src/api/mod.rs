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

use crate::artifacts::store::{ArtifactStore, Manifest};
use crate::auth::{self, Actor, Role, Surface, Tokens};
use crate::catalog::Catalog;
use crate::config::PushWindow;
use crate::pipeline::Pipeline;
use crate::policy::{self, Action, PlayerCount, Situation};
use crate::servers::{Manager, ServerError};
use crate::store::Database;
use crate::templates::Templates;

/// How big an uploaded jar may be. Geyser is around sixty megabytes and
/// nothing sensible is anywhere near this.
const MAX_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

pub struct AppState {
    pub manager: Arc<Manager>,
    pub database: Arc<Database>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// The parts of the policy situation that come from the config file.
    pub frozen_servers: Vec<String>,
    pub non_minecraft_servers: Vec<String>,
    pub player_count_url: Option<String>,
    pub push_window: Option<PushWindow>,
    /// The catalog and templates, reloadable without a restart because
    /// they come from a git checkout somebody else may be updating.
    pub config: tokio::sync::RwLock<LoadedConfig>,
    pub manifest: Arc<dyn Manifest>,
    pub store: Arc<dyn ArtifactStore>,
    pub resolver: Arc<crate::artifacts::sources::Resolver>,
    pub config_directory: std::path::PathBuf,
    pub build_directory: std::path::PathBuf,
    pub workspace: std::path::PathBuf,
    /// The config repository, when the configuration is in git.
    pub repo: Option<crate::gitconfig::ConfigRepo>,
    pub notifier: crate::notify::Notifier,
    /// One pipeline at a time. Two runs fetching the same artifacts into
    /// the same store is a race nobody needs to debug.
    pub pipeline_lock: tokio::sync::Mutex<()>,
}

pub struct LoadedConfig {
    pub catalog: Catalog,
    pub templates: Templates,
}

impl AppState {
    /// Assembles what the policy layer needs to know right now.
    pub async fn situation(&self) -> Situation {
        Situation {
            frozen_servers: self.frozen_servers.clone(),
            non_minecraft_servers: self.non_minecraft_servers.clone(),
            players_online: self.players_online().await,
            push_window_open: self
                .push_window
                .as_ref()
                .map(|window| window.is_open_at(chrono::Utc::now()))
                .unwrap_or(true),
        }
    }

    /// Asks the configured endpoint how many players are connected.
    ///
    /// A slow answer is the same as no answer here: the point of the count
    /// is to decide quickly whether a restart is rude, and a request that
    /// hangs for thirty seconds has already failed at that.
    async fn players_online(&self) -> PlayerCount {
        let Some(url) = self.player_count_url.as_deref() else {
            return PlayerCount::Unconfigured;
        };
        let response = self
            .resolver
            .client()
            .get(url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let body = match response {
            Ok(response) if response.status().is_success() => response.text().await,
            Ok(response) => {
                tracing::warn!("player count endpoint answered {}", response.status());
                return PlayerCount::Unavailable;
            }
            Err(e) => {
                tracing::warn!("could not reach the player count endpoint: {e}");
                return PlayerCount::Unavailable;
            }
        };
        match body.ok().as_deref().and_then(parse_player_count) {
            Some(count) => PlayerCount::Known(count),
            None => PlayerCount::Unavailable,
        }
    }

    /// Decides, writes it down, and gives the caller the reason either way.
    pub async fn authorize(
        &self,
        actor: &Actor,
        action: Action,
        force: bool,
    ) -> Result<(), ApiError> {
        let situation = self.situation().await;
        let verdict = policy::check(actor, &action, &situation, force);
        auth::audit(
            &self.database,
            actor,
            action.name(),
            action.target().as_deref(),
            if verdict.allowed { "allowed" } else { "denied" },
            Some(&verdict.reason),
        )
        .await;
        if verdict.allowed {
            Ok(())
        } else {
            Err(ApiError::Denied(verdict))
        }
    }

    pub async fn run_pipeline(&self) -> Result<crate::pipeline::PipelineReport, ApiError> {
        let Ok(_guard) = self.pipeline_lock.try_lock() else {
            return Err(ApiError::Busy("a pipeline run is already in progress"));
        };
        // The pipeline itself runs whenever, but the branches it would push
        // are subject to the push window like any other push. Outside it
        // the run still fetches and builds, it just proposes nothing.
        let repo = match self.repo.as_ref() {
            None => None,
            Some(repo) => {
                let verdict = policy::check(
                    &Actor::system(),
                    &Action::PushToGit {
                        repository: "the config repository".into(),
                    },
                    &self.situation().await,
                    false,
                );
                if verdict.allowed {
                    Some(repo)
                } else {
                    tracing::info!("not proposing pin bumps this run: {}", verdict.reason);
                    None
                }
            }
        };
        let config = self.config.read().await;
        let pipeline = Pipeline {
            catalog: &config.catalog,
            templates: &config.templates,
            manifest: self.manifest.as_ref(),
            store: self.store.as_ref(),
            resolver: self.resolver.as_ref(),
            output: self.build_directory.clone(),
            workspace: self.workspace.clone(),
            repo,
        };
        let report = pipeline.run().await;
        self.notifier.pipeline_finished(&report).await;
        Ok(report)
    }
}

/// Reads a player count out of whatever the endpoint answered with.
fn parse_player_count(body: &str) -> Option<u32> {
    let trimmed = body.trim();
    if let Ok(count) = trimmed.parse::<u32>() {
        return Some(count);
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    value
        .pointer("/players/online")
        .or_else(|| value.pointer("/online"))
        .or_else(|| value.pointer("/players"))
        .and_then(|found| found.as_u64())
        .map(|count| count as u32)
}

/// Errors that are not about a server.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Busy(&'static str),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Internal(String),
    #[error("{}", .0.reason)]
    Denied(policy::Verdict),
    #[error("{0}")]
    Unauthenticated(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::Busy(_) => StatusCode::CONFLICT,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            // A refusal that the caller could override is not the same as
            // one they cannot, and the status says which: 409 means try
            // again with force, 403 means do not bother.
            ApiError::Denied(verdict) if verdict.overridable => StatusCode::CONFLICT,
            ApiError::Denied(_) => StatusCode::FORBIDDEN,
        };
        let body = match &self {
            ApiError::Denied(verdict) => json!({
                "error": verdict.reason,
                "overridable": verdict.overridable,
            }),
            other => json!({ "error": other.to_string() }),
        };
        (status, Json(body)).into_response()
    }
}

/// The caller, resolved from their token.
///
/// Every handler that changes anything takes one of these, so forgetting to
/// authenticate is a compile error rather than a hole.
pub struct Caller(pub Actor);

impl axum::extract::FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or("");
        let actor = Tokens::new(&state.database)
            .verify(presented, Surface::Api)
            .await
            .map_err(|e| ApiError::Unauthenticated(e.to_string()))?;
        Ok(Caller(actor))
    }
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
        .route("/api/v1/servers/{name}/preflight", get(preflight))
        .route("/api/v1/servers/{name}/start", post(start_server))
        .route("/api/v1/servers/{name}/stop", post(stop_server))
        .route("/api/v1/servers/{name}/restart", post(restart_server))
        .route("/api/v1/servers/{name}/command", post(run_command))
        .route("/api/v1/servers/{name}/console", get(console))
        .route("/api/v1/servers/{name}/backup", post(backup))
        .route("/api/v1/artifacts", get(list_artifacts))
        .route(
            "/api/v1/artifacts/{name}/versions",
            get(artifact_versions)
                .post(upload_version)
                // curl -T is a PUT, and that is how everybody sends a file.
                .put(upload_version)
                // A jar is bigger than the two megabytes axum allows by
                // default. FastAsyncWorldEdit alone is thirty.
                .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/api/v1/templates", get(list_templates))
        .route("/api/v1/templates/{name}/build", post(build_template))
        .route("/api/v1/pipeline/run", post(run_pipeline))
        .route("/api/v1/config/reload", post(reload_config))
        .route("/api/v1/tokens", get(list_tokens).post(create_token))
        .route("/api/v1/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/api/v1/audit", get(list_audit))
        .route("/api/v1/whoami", get(whoami))
        .route("/mcp", post(crate::mcp::handle))
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

/// Reading needs a token but no particular power. Separate from
/// `authorize` because writing an audit row per page refresh would drown
/// the interesting lines.
fn require_read(actor: &Actor) -> Result<(), ApiError> {
    if actor.role >= Role::Viewer {
        Ok(())
    } else {
        Err(ApiError::Unauthenticated("that token cannot read".into()))
    }
}

impl From<ServerError> for ApiError {
    fn from(error: ServerError) -> Self {
        match error {
            ServerError::Unknown(name) => ApiError::BadRequest(format!("no server named {name}")),
            other => ApiError::Internal(other.to_string()),
        }
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

async fn list_servers(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
) -> Result<impl IntoResponse, ApiError> {
    require_read(&actor)?;
    Ok(Json(state.manager.list().await))
}

async fn server_status(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    require_read(&actor)?;
    Ok(Json(state.manager.status(&name).await?).into_response())
}

/// What the policy layer would say, without doing anything.
///
/// The web UI asks before it draws the button, so the operator reads "would
/// drop 2 players" instead of finding out afterwards.
async fn preflight(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_read(&actor)?;
    let situation = state.situation().await;
    let verdicts: serde_json::Map<String, serde_json::Value> = [
        (
            "start",
            Action::Start {
                server: name.clone(),
            },
        ),
        (
            "stop",
            Action::Stop {
                server: name.clone(),
            },
        ),
        (
            "restart",
            Action::Restart {
                server: name.clone(),
            },
        ),
        (
            "command",
            Action::SendCommand {
                server: name.clone(),
            },
        ),
        (
            "backup",
            Action::Backup {
                server: name.clone(),
            },
        ),
    ]
    .into_iter()
    .map(|(label, action)| {
        let verdict = policy::check(&actor, &action, &situation, false);
        (label.to_string(), serde_json::to_value(verdict).unwrap())
    })
    .collect();
    Ok(Json(json!({ "server": name, "actions": verdicts })))
}

async fn start_server(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    state
        .authorize(
            &actor,
            Action::Start {
                server: name.clone(),
            },
            false,
        )
        .await?;
    state.manager.start(&name).await?;
    Ok(Json(json!({ "started": name })).into_response())
}

#[derive(Deserialize, Default)]
struct ForceBody {
    #[serde(default)]
    force: bool,
}

async fn stop_server(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
    body: Option<Json<ForceBody>>,
) -> Result<Response, ApiError> {
    let force = body.map(|Json(b)| b.force).unwrap_or(false);
    state
        .authorize(
            &actor,
            Action::Stop {
                server: name.clone(),
            },
            force,
        )
        .await?;
    state.manager.stop(&name).await?;
    Ok(Json(json!({ "stopped": name })).into_response())
}

#[derive(Deserialize, Default)]
struct RestartBody {
    #[serde(default = "default_true")]
    update_template: bool,
    /// Restart anyway, players and all.
    #[serde(default)]
    force: bool,
    /// Minutes of warning before the server goes down. Absent means the
    /// configured default, and zero means right now.
    #[serde(default)]
    warn_minutes: Option<u64>,
}
fn default_true() -> bool {
    true
}

async fn restart_server(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
    body: Option<Json<RestartBody>>,
) -> Result<Response, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    state
        .authorize(
            &actor,
            Action::Restart {
                server: name.clone(),
            },
            body.force,
        )
        .await?;
    // Applying a template is a second thing that can be refused on its own,
    // because a server may be startable but not templatable.
    if body.update_template {
        state
            .authorize(
                &actor,
                Action::ApplyTemplate {
                    server: name.clone(),
                },
                body.force,
            )
            .await?;
    }
    let warned = state
        .manager
        .restart_with_warning(&name, body.update_template, body.warn_minutes)
        .await?;
    Ok(Json(json!({
        "restarted": name,
        "update_template": body.update_template,
        "warned_for_minutes": warned,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct CommandBody {
    command: String,
}

async fn run_command(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
    Json(body): Json<CommandBody>,
) -> Result<Response, ApiError> {
    policy::check_command(&body.command).map_err(ApiError::BadRequest)?;
    state
        .authorize(
            &actor,
            Action::SendCommand {
                server: name.clone(),
            },
            false,
        )
        .await?;
    state.manager.run_command(&name, &body.command).await?;
    Ok(Json(json!({ "sent": body.command, "to": name })).into_response())
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
    Caller(actor): Caller,
    Path(name): Path<String>,
    Query(query): Query<ConsoleQuery>,
) -> Result<Response, ApiError> {
    require_read(&actor)?;
    let text = state.manager.console(&name, query.lines.min(5000)).await?;
    Ok(Json(json!({
        "untrusted": true,
        "label": format!("console output from {name}"),
        "content": text,
    }))
    .into_response())
}

async fn backup(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    state
        .authorize(
            &actor,
            Action::Backup {
                server: name.clone(),
            },
            false,
        )
        .await?;
    let report = state.manager.backup(&name).await?;
    Ok(Json(json!({ "backup": report })).into_response())
}

async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
) -> Result<impl IntoResponse, ApiError> {
    require_read(&actor)?;
    let config = state.config.read().await;
    Ok(Json(json!({ "artifacts": config.catalog.artifacts })))
}

async fn artifact_versions(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_read(&actor)?;
    let versions = state
        .manifest
        .versions(&name)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // A manifest row whose bytes have gone missing is a real thing that
    // happens, and finding out at build time is too late.
    let mut out = Vec::with_capacity(versions.len());
    for version in versions {
        let present = state.store.exists(&version.key).await.unwrap_or(false);
        let mut value = serde_json::to_value(&version).unwrap_or_default();
        if let Some(object) = value.as_object_mut() {
            object.insert("present".into(), json!(present));
        }
        out.push(value);
    }
    Ok(Json(json!({ "artifact": name, "versions": out })))
}

async fn list_templates(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
) -> Result<impl IntoResponse, ApiError> {
    require_read(&actor)?;
    let config = state.config.read().await;
    let templates: Vec<_> = config
        .templates
        .by_name
        .values()
        .map(|template| {
            let resolved = config.templates.resolved(&template.name).ok();
            json!({
                "name": template.name,
                "buildable": resolved.as_ref().map(|r| r.buildable).unwrap_or(false),
                "inherits": template.def.inherits,
                "artifacts": resolved.map(|r| r.artifacts).unwrap_or_default(),
            })
        })
        .collect();
    Ok(Json(json!({ "templates": templates })))
}

async fn build_template(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .authorize(&actor, Action::BuildTemplate { name: name.clone() }, false)
        .await?;
    let config = state.config.read().await;
    let pipeline = Pipeline {
        catalog: &config.catalog,
        templates: &config.templates,
        manifest: state.manifest.as_ref(),
        store: state.store.as_ref(),
        resolver: state.resolver.as_ref(),
        output: state.build_directory.clone(),
        workspace: state.workspace.clone(),
        repo: state.repo.as_ref(),
    };
    let outcome = pipeline.build_one(&name).await;
    if outcome.succeeded {
        Ok(Json(json!({ "build": outcome })))
    } else {
        Err(ApiError::BadRequest(
            outcome.error.unwrap_or_else(|| "build failed".into()),
        ))
    }
}

async fn run_pipeline(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
) -> Result<impl IntoResponse, ApiError> {
    state.authorize(&actor, Action::RunPipeline, false).await?;
    let report = state.run_pipeline().await?;
    Ok(Json(json!({ "pipeline": report })))
}

async fn reload_config(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
) -> Result<impl IntoResponse, ApiError> {
    state.authorize(&actor, Action::RunPipeline, false).await?;
    let (catalog, templates) = crate::pipeline::load_config(&state.config_directory)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let artifacts = catalog.artifacts.len();
    let template_count = templates.by_name.len();
    *state.config.write().await = LoadedConfig { catalog, templates };
    Ok(Json(json!({
        "reloaded": true,
        "artifacts": artifacts,
        "templates": template_count,
    })))
}

async fn whoami(Caller(actor): Caller) -> impl IntoResponse {
    Json(json!({
        "name": actor.name,
        "role": actor.role,
        "surface": actor.surface,
        "player_uuid": actor.player_uuid,
    }))
}

async fn list_tokens(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
) -> Result<impl IntoResponse, ApiError> {
    state.authorize(&actor, Action::ManageTokens, false).await?;
    let tokens = Tokens::new(&state.database)
        .list()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "tokens": tokens })))
}

#[derive(Deserialize)]
struct NewTokenBody {
    name: String,
    role: String,
}

async fn create_token(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Json(body): Json<NewTokenBody>,
) -> Result<impl IntoResponse, ApiError> {
    state.authorize(&actor, Action::ManageTokens, false).await?;
    let role = Role::parse(&body.role)
        .ok_or_else(|| ApiError::BadRequest(format!("{:?} is not a role", body.role)))?;
    let (record, secret) = Tokens::new(&state.database)
        .create(body.name.trim(), role)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // The only time the plaintext exists outside the caller's hands.
    Ok(Json(json!({ "token": record, "secret": secret })))
}

async fn revoke_token(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    state.authorize(&actor, Action::ManageTokens, false).await?;
    let revoked = Tokens::new(&state.database)
        .revoke(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "revoked": revoked, "id": id })))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: i64,
}
fn default_audit_limit() -> i64 {
    200
}

async fn list_audit(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Query(query): Query<AuditQuery>,
) -> Result<impl IntoResponse, ApiError> {
    // Reading the log is an admin thing: it names everybody and what they
    // tried, which is more than a viewer needs.
    state.authorize(&actor, Action::ManageTokens, false).await?;
    let entries = auth::recent_audit(&state.database, query.limit)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "audit": entries })))
}

#[derive(Deserialize)]
struct UploadQuery {
    /// The version these bytes are. Read out of the jar when left off.
    version: Option<String>,
}

/// Puts a jar into the store by hand.
///
/// Some plugins live behind a login and a bot check, and no amount of
/// cleverness makes downloading them reliable. Rather than have the whole
/// pipeline depend on beating a challenge page, those get uploaded here and
/// then behave like everything else: pinned, resolved, built and deployed
/// by the same code.
///
/// Send the jar as the body, nothing else:
///
///     curl -T CMI9.8.1.jar -H "Authorization: Bearer $TOKEN" \
///          "http://localhost:8484/api/v1/artifacts/CMI/versions"
async fn upload_version(
    State(state): State<Arc<AppState>>,
    Caller(actor): Caller,
    Path(name): Path<String>,
    Query(query): Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    state
        .authorize(&actor, Action::BuildTemplate { name: name.clone() }, false)
        .await?;
    if body.is_empty() {
        return Err(ApiError::BadRequest("that upload was empty".into()));
    }

    let known = {
        let config = state.config.read().await;
        config
            .catalog
            .artifacts
            .iter()
            .find(|artifact| artifact.name == name)
            .cloned()
    };
    let Some(artifact) = known else {
        // Storing a jar under a name nothing pins would leave bytes on disk
        // that no build ever looks at.
        return Err(ApiError::BadRequest(format!(
            "{name} is not in the catalog, so nothing would ever use this file"
        )));
    };

    let version = match query.version {
        Some(version) if !version.trim().is_empty() => version.trim().to_string(),
        // Reading it out of the jar is better than trusting a filename,
        // which is how the old tooling got Multiverse wrong for months.
        _ => crate::artifacts::jarmeta::read_plugin_meta(&body)
            .map(|meta| meta.version)
            .map_err(|e| {
                ApiError::BadRequest(format!(
                    "could not find a version in that jar ({e}), so pass one as ?version="
                ))
            })?,
    };

    let outcome = crate::artifacts::store_bytes(
        state.store.as_ref(),
        state.manifest.as_ref(),
        &artifact,
        &version,
        &body,
        "uploaded by hand",
        None,
    )
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "stored": outcome })))
}
