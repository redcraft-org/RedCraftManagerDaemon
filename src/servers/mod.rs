//! Server supervision: discovery, lifecycle, healthcheck.
//!
//! The structural differences from rcsm, each fixing a specific problem
//! found in the Go code:
//! - One async mutex **per server** instead of one global lock: rcsm held
//!   `minecraftServersLock` across a stop that can block for two minutes,
//!   which serialized the whole fleet behind one slow server.
//! - Operations return `Result` instead of logging and returning nothing, so
//!   an API handler has something to turn into a status code.
//! - No process exits in library code (rcsm had five).
//!
//! The behavioral contract that is deliberately identical:
//! - Crash revive pulls a fresh template; a plain restart does not. Boot
//!   only applies a template when the session does not already exist.
//! - Bootloop detection: more than `crash_max_tries` revives inside
//!   `crash_window_secs` and the server is left down for a human.

pub mod apply;
pub mod backup;
pub mod tmux;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};

use crate::config::{BackupDestination, Config, ServerConfig, TemplateSource};
use crate::domain::{ServerState, ServerStatus};
use tmux::Tmux;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("unknown server {0:?}")]
    Unknown(String),
    #[error("server {0} is already running")]
    AlreadyRunning(String),
    #[error("server {0} is not running")]
    NotRunning(String),
    #[error(transparent)]
    Tmux(#[from] tmux::TmuxError),
    #[error(transparent)]
    Apply(#[from] apply::ApplyError),
    #[error(transparent)]
    Backup(#[from] backup::BackupError),
    #[error("{0}")]
    Config(String),
}

/// The crash/bootloop arithmetic from rcsm's healthcheck, extracted pure so
/// it can be tested without a clock or a tmux.
///
/// A crash outside the window starts a fresh window. More than `max_tries`
/// crashes inside one window is a bootloop.
#[derive(Debug, Clone)]
pub struct CrashTracker {
    max_tries: u32,
    window: Duration,
    tries: u32,
    window_start: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CrashDecision {
    Revive { attempt: u32 },
    GiveUp,
}

impl CrashTracker {
    pub fn new(max_tries: u32, window: Duration) -> Self {
        Self {
            max_tries,
            window,
            tries: 0,
            window_start: None,
        }
    }

    pub fn record_crash(&mut self, now: Instant) -> CrashDecision {
        match self.window_start {
            Some(start) if now.duration_since(start) <= self.window => {}
            _ => {
                self.window_start = Some(now);
                self.tries = 0;
            }
        }
        self.tries += 1;
        if self.tries > self.max_tries {
            CrashDecision::GiveUp
        } else {
            CrashDecision::Revive {
                attempt: self.tries,
            }
        }
    }

    /// A successful manual start clears the history: the human took over.
    pub fn reset(&mut self) {
        self.tries = 0;
        self.window_start = None;
    }

    pub fn tries(&self) -> u32 {
        self.tries
    }
}

struct ServerEntry {
    dir: PathBuf,
    /// Serializes lifecycle operations for this one server.
    lifecycle: Mutex<()>,
    state: RwLock<ServerState>,
    crashes: Mutex<CrashTracker>,
}

pub struct Manager {
    tmux: Tmux,
    servers: RwLock<HashMap<String, Arc<ServerEntry>>>,
    servers_directory: PathBuf,
    stop_timeout: Duration,
    crash_max_tries: u32,
    crash_window: Duration,
    template_source: TemplateSource,
    backup_destination: BackupDestination,
}

impl Manager {
    pub fn new(config: &Config) -> Self {
        Self {
            tmux: Tmux::new(config.tmux_session_prefix.clone()),
            servers: RwLock::new(HashMap::new()),
            servers_directory: config.servers_directory.clone(),
            stop_timeout: Duration::from_secs(config.stop_timeout_secs),
            crash_max_tries: config.crash_max_tries,
            crash_window: Duration::from_secs(config.crash_window_secs),
            template_source: config.template_source.clone(),
            backup_destination: config.backup_destination.clone(),
        }
    }

    /// Discovery + adoption, rcsm's boot sequence with the same rules:
    /// every directory is a server; a live session is adopted as Running and
    /// its template is NOT touched; a dead server gets a template refresh
    /// and stays stopped until something starts it.
    pub async fn discover(&self) -> anyhow::Result<Vec<String>> {
        let mut found = Vec::new();
        let read = std::fs::read_dir(&self.servers_directory).map_err(|e| {
            anyhow::anyhow!("could not list {}: {e}", self.servers_directory.display())
        })?;

        for entry in read.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();

            // Make sure the per-server config exists (writes the default for
            // a brand-new directory, as rcsm did).
            if let Err(e) = ServerConfig::read_or_init(&dir) {
                tracing::error!("skipping {name}: {e}");
                continue;
            }

            let alive = self.tmux.session_exists(&name).await.unwrap_or(false);
            let state = if alive {
                tracing::info!("adopted running session for {name}");
                ServerState::Running
            } else {
                if let Err(e) = self.update_template(&name, &dir).await {
                    tracing::warn!("template refresh for {name} failed: {e}");
                }
                ServerState::Stopped
            };

            self.servers.write().await.insert(
                name.clone(),
                Arc::new(ServerEntry {
                    dir,
                    lifecycle: Mutex::new(()),
                    state: RwLock::new(state),
                    crashes: Mutex::new(CrashTracker::new(self.crash_max_tries, self.crash_window)),
                }),
            );
            found.push(name);
        }
        found.sort();
        Ok(found)
    }

    async fn entry(&self, name: &str) -> Result<Arc<ServerEntry>, ServerError> {
        self.servers
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| ServerError::Unknown(name.to_string()))
    }

    pub async fn list(&self) -> Vec<ServerStatus> {
        let servers = self.servers.read().await;
        let mut out = Vec::with_capacity(servers.len());
        for (name, entry) in servers.iter() {
            out.push(ServerStatus {
                name: name.clone(),
                state: *entry.state.read().await,
                session_alive: self.tmux.session_exists(name).await.unwrap_or(false),
                restart_tries: entry.crashes.lock().await.tries(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub async fn status(&self, name: &str) -> Result<ServerStatus, ServerError> {
        let entry = self.entry(name).await?;
        let state = *entry.state.read().await;
        let restart_tries = entry.crashes.lock().await.tries();
        let session_alive = self.tmux.session_exists(name).await?;
        Ok(ServerStatus {
            name: name.to_string(),
            state,
            session_alive,
            restart_tries,
        })
    }

    /// Applies the template for a server if a source is configured and a
    /// template exists. Not finding one is not an error: a server without a
    /// template is simply unmanaged disk state, which is the newbie default.
    async fn update_template(
        &self,
        name: &str,
        dir: &std::path::Path,
    ) -> Result<bool, ServerError> {
        match &self.template_source {
            TemplateSource::None => Ok(false),
            TemplateSource::LocalDir { path } => {
                let tar = path.join(format!("{name}.tar"));
                if !tar.exists() {
                    return Ok(false);
                }
                // to_path_buf, not clone: `dir` is a &Path here, and cloning
                // a reference would hand the blocking task a borrow it cannot
                // outlive.
                let dir = dir.to_path_buf();
                let report = tokio::task::spawn_blocking(move || apply::apply_tar(&tar, &dir))
                    .await
                    .map_err(|e| ServerError::Config(format!("apply task panicked: {e}")))?;
                let report = report?;
                tracing::info!(
                    "applied template for {name}: {} files, {} dirs replaced",
                    report.files_written,
                    report.dirs_replaced
                );
                Ok(true)
            }
        }
    }

    /// Operator-initiated start. Clears the crash history, because a human
    /// deciding to start a server is the signal that whatever was wrong has
    /// been dealt with; that is also the only way out of a bootloop.
    pub async fn start(&self, name: &str) -> Result<(), ServerError> {
        let result = self.start_inner(name).await;
        if result.is_ok() {
            self.entry(name).await?.crashes.lock().await.reset();
        }
        result
    }

    /// The start the healthcheck uses. It must NOT touch the crash history:
    /// a bootlooping server starts successfully every time and dies a second
    /// later, so resetting here made the counter stick at one and the fleet
    /// would revive a dead server forever.
    async fn start_inner(&self, name: &str) -> Result<(), ServerError> {
        let entry = self.entry(name).await?;
        let _guard = entry.lifecycle.lock().await;

        if self.tmux.session_exists(name).await? {
            *entry.state.write().await = ServerState::Running;
            return Err(ServerError::AlreadyRunning(name.to_string()));
        }

        let config =
            ServerConfig::read(&entry.dir).map_err(|e| ServerError::Config(e.to_string()))?;

        *entry.state.write().await = ServerState::Starting;
        match self
            .tmux
            .session_create(name, &entry.dir, &config.start_command)
            .await
        {
            Ok(()) => {
                *entry.state.write().await = ServerState::Running;
                tracing::info!("started {name}");
                Ok(())
            }
            Err(e) => {
                *entry.state.write().await = ServerState::Crashed { bootloop: false };
                Err(e.into())
            }
        }
    }

    pub async fn stop(&self, name: &str) -> Result<(), ServerError> {
        let entry = self.entry(name).await?;
        let _guard = entry.lifecycle.lock().await;

        if !self.tmux.session_exists(name).await? {
            *entry.state.write().await = ServerState::Stopped;
            return Err(ServerError::NotRunning(name.to_string()));
        }
        let config =
            ServerConfig::read(&entry.dir).map_err(|e| ServerError::Config(e.to_string()))?;

        // Marked before the stop command goes out: the healthcheck must not
        // read the session's death as a crash and revive what we are killing.
        *entry.state.write().await = ServerState::Stopping;
        let killed = self
            .tmux
            .session_terminate(name, &config.stop_command, self.stop_timeout)
            .await?;
        if killed {
            tracing::warn!(
                "timeout shutdown of {}s reached for {name}, killed the session",
                self.stop_timeout.as_secs()
            );
        }
        *entry.state.write().await = ServerState::Stopped;
        tracing::info!("stopped {name}");
        Ok(())
    }

    /// Restart, with the template question explicit instead of encoded in
    /// which code path kills the process (the rcsm trick this replaces).
    pub async fn restart(&self, name: &str, update_template: bool) -> Result<(), ServerError> {
        match self.stop(name).await {
            Ok(()) | Err(ServerError::NotRunning(_)) => {}
            Err(e) => return Err(e),
        }
        if update_template {
            let entry = self.entry(name).await?;
            let _guard = entry.lifecycle.lock().await;
            self.update_template(name, &entry.dir).await?;
        }
        self.start(name).await
    }

    pub async fn run_command(&self, name: &str, command: &str) -> Result<(), ServerError> {
        let entry = self.entry(name).await?;
        if !self.tmux.session_exists(name).await? {
            return Err(ServerError::NotRunning(name.to_string()));
        }
        let _ = entry; // command delivery needs no lifecycle lock
        self.tmux.session_run_command(name, command).await?;
        Ok(())
    }

    pub async fn console(&self, name: &str, lines: u32) -> Result<String, ServerError> {
        self.entry(name).await?;
        if !self.tmux.session_exists(name).await? {
            return Err(ServerError::NotRunning(name.to_string()));
        }
        Ok(self.tmux.capture_pane(name, lines).await?)
    }

    pub async fn backup(&self, name: &str) -> Result<backup::BackupReport, ServerError> {
        let entry = self.entry(name).await?;
        let config =
            ServerConfig::read(&entry.dir).map_err(|e| ServerError::Config(e.to_string()))?;
        let BackupDestination::LocalDir { path } = &self.backup_destination;
        let dir = entry.dir.clone();
        let dest = path.clone();
        let report = tokio::task::spawn_blocking(move || {
            backup::backup_server(
                &dir.file_name().unwrap().to_string_lossy(),
                &dir,
                &config.directories_to_backup,
                &dest,
            )
        })
        .await
        .map_err(|e| ServerError::Config(format!("backup task panicked: {e}")))??;
        tracing::info!("backed up {name}: {} bytes", report.bytes);
        Ok(report)
    }

    /// One healthcheck sweep: every server rcmd believes is running whose
    /// session has vanished is a crash. Revive pulls a template, exactly as
    /// rcsm's revive did; too many revives in the window and the server is
    /// left down as a bootloop.
    pub async fn healthcheck_sweep(&self) {
        let names: Vec<(String, Arc<ServerEntry>)> = self
            .servers
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for (name, entry) in names {
            let believed = *entry.state.read().await;
            if believed != ServerState::Running {
                continue;
            }
            let alive = match self.tmux.session_exists(&name).await {
                Ok(alive) => alive,
                Err(e) => {
                    tracing::warn!("healthcheck could not query tmux for {name}: {e}");
                    continue;
                }
            };
            if alive {
                continue;
            }

            let decision = entry.crashes.lock().await.record_crash(Instant::now());
            match decision {
                CrashDecision::GiveUp => {
                    tracing::error!("server crash bootloop detected for {name}, giving up");
                    *entry.state.write().await = ServerState::Crashed { bootloop: true };
                }
                CrashDecision::Revive { attempt } => {
                    tracing::warn!("{name} is stopped, restarting (attempt {attempt})");
                    *entry.state.write().await = ServerState::Crashed { bootloop: false };
                    if let Err(e) = self.update_template(&name, &entry.dir).await {
                        tracing::warn!("template refresh during revive of {name} failed: {e}");
                    }
                    if let Err(e) = self.start_inner(&name).await {
                        tracing::error!("revive of {name} failed: {e}");
                    }
                }
            }
        }
    }

    /// The 1-second ticker, rcsm's cadence.
    pub fn spawn_healthcheck(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                manager.healthcheck_sweep().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootloop_math_matches_rcsm() {
        // rcsm: max 3 tries inside a 120s window; the 4th is a bootloop.
        let mut tracker = CrashTracker::new(3, Duration::from_secs(120));
        let t0 = Instant::now();
        assert_eq!(
            tracker.record_crash(t0),
            CrashDecision::Revive { attempt: 1 }
        );
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(10)),
            CrashDecision::Revive { attempt: 2 }
        );
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(20)),
            CrashDecision::Revive { attempt: 3 }
        );
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(30)),
            CrashDecision::GiveUp
        );
    }

    #[test]
    fn a_crash_after_the_window_starts_fresh() {
        let mut tracker = CrashTracker::new(3, Duration::from_secs(120));
        let t0 = Instant::now();
        tracker.record_crash(t0);
        tracker.record_crash(t0 + Duration::from_secs(1));
        // Well past the window: a stable server that eventually crashed
        // again is not bootlooping.
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(500)),
            CrashDecision::Revive { attempt: 1 }
        );
    }

    /// The bug a live bootloop found: a crashing server starts *successfully*
    /// every time and dies a second later, so resetting the counter on a
    /// successful start pinned it at one and the healthcheck revived forever.
    /// Only an operator-initiated start may clear the history.
    #[test]
    fn a_successful_revive_start_does_not_clear_the_history() {
        let mut tracker = CrashTracker::new(3, Duration::from_secs(120));
        let t0 = Instant::now();
        // Each cycle: crash, then a revive that starts fine. No reset.
        assert_eq!(
            tracker.record_crash(t0),
            CrashDecision::Revive { attempt: 1 }
        );
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(1)),
            CrashDecision::Revive { attempt: 2 }
        );
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(2)),
            CrashDecision::Revive { attempt: 3 }
        );
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(3)),
            CrashDecision::GiveUp
        );
    }

    #[test]
    fn manual_start_resets_history() {
        let mut tracker = CrashTracker::new(3, Duration::from_secs(120));
        let t0 = Instant::now();
        tracker.record_crash(t0);
        tracker.record_crash(t0 + Duration::from_secs(1));
        tracker.record_crash(t0 + Duration::from_secs(2));
        tracker.reset();
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(3)),
            CrashDecision::Revive { attempt: 1 }
        );
    }
}
