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

/// What a fleet wide job did.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FleetReport {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warned: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub restarted: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backed_up: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
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
    /// What to send before a restart, with `{minutes}` and `{reason}`.
    warn_command: String,
    warn_seconds: u64,
    /// Opened once at startup, because opening a client per apply would
    /// mean a template download that is mostly handshake.
    template_bucket: Option<Arc<crate::s3::Bucket>>,
    backup_bucket: Option<Arc<crate::s3::Bucket>>,
    /// Where a crash goes when nobody is looking at the log.
    notifier: Option<Arc<crate::notify::Notifier>>,
    /// Servers the automatic paths must not touch.
    ///
    /// The policy layer already refuses to restart these when a person
    /// asks, but the healthcheck never goes through the policy layer, and
    /// the healthcheck is the thing most likely to touch a server at 4am.
    /// A frozen server that gets revived and re-templated automatically is
    /// not frozen in any sense that matters.
    frozen: Vec<String>,
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
            template_bucket: None,
            backup_bucket: None,
            notifier: None,
            frozen: config.frozen_servers.clone(),
            warn_command: config.warn_command.clone(),
            warn_seconds: config.warn_seconds,
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

            // A tmux query that FAILED is not the same as a server that is
            // stopped, and the difference is not academic: applying a
            // template deletes and rewrites every directory the tar names.
            // Getting this wrong once, by running as the wrong user or with
            // a private /tmp, would wipe plugins/ out from under eight
            // running servers. So an error leaves the server alone.
            let state = match self.tmux.session_exists(&name).await {
                Ok(true) => {
                    tracing::info!("adopted running session for {name}");
                    ServerState::Running
                }
                Ok(false) => {
                    if let Err(e) = self.update_template(&name, &dir).await {
                        tracing::warn!("template refresh for {name} failed: {e}");
                    }
                    ServerState::Stopped
                }
                Err(e) => {
                    tracing::error!(
                        "could not ask tmux about {name} ({e}). Leaving it alone: \
                         rcmd will not touch a server it cannot see."
                    );
                    ServerState::Stopped
                }
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
            TemplateSource::S3 { .. } => {
                let Some(bucket) = self.template_bucket.as_ref() else {
                    return Err(ServerError::Config(
                        "the template bucket is configured but was never opened".into(),
                    ));
                };
                let key = crate::s3::template_key(name);
                let bytes = match bucket.get(&key).await {
                    Ok(bytes) => bytes,
                    // A server with no template in the bucket is not a
                    // failure. It is a server nobody templates.
                    Err(crate::s3::S3Error::Missing { .. }) => {
                        tracing::warn!("no template at {}/{key}", bucket.name());
                        return Ok(false);
                    }
                    Err(e) => return Err(ServerError::Config(e.to_string())),
                };

                let dir = dir.to_path_buf();
                let size = bytes.len();
                let report = tokio::task::spawn_blocking(move || {
                    apply::apply_tar_reader(std::io::Cursor::new(bytes), &dir)
                })
                .await
                .map_err(|e| ServerError::Config(format!("apply task panicked: {e}")))?;
                let report = report?;
                tracing::info!(
                    "applied template for {name} from {}/{key} ({size} bytes): {} files, {} dirs replaced",
                    bucket.name(),
                    report.files_written,
                    report.dirs_replaced
                );
                Ok(true)
            }
        }
    }

    /// Opens whichever buckets the config asks for.
    ///
    /// Called once at startup and separate from `new` because it needs the
    /// runtime and can fail. A bucket that will not open is fatal on
    /// purpose: starting anyway would mean a daemon that silently stops
    /// applying templates, and finding that out at the next crash revive
    /// is the worst possible moment.
    /// Hands the manager somewhere to shout when a server falls over.
    pub fn set_notifier(&mut self, notifier: Arc<crate::notify::Notifier>) {
        self.notifier = Some(notifier);
    }

    async fn tell(&self, level: crate::notify::Level, server: &str, message: &str) {
        if let Some(notifier) = &self.notifier {
            notifier.event(level, server, message).await;
        }
    }

    pub async fn open_buckets(&mut self) -> Result<(), ServerError> {
        if let TemplateSource::S3 { bucket } = &self.template_source {
            let opened = crate::s3::Bucket::connect(bucket)
                .await
                .map_err(|e| ServerError::Config(format!("template bucket: {e}")))?;
            tracing::info!("templates come from {}", opened.name());
            self.template_bucket = Some(Arc::new(opened));
        }
        if let BackupDestination::S3 { bucket, .. } = &self.backup_destination {
            let opened = crate::s3::Bucket::connect(bucket)
                .await
                .map_err(|e| ServerError::Config(format!("backup bucket: {e}")))?;
            tracing::info!("backups go to {}", opened.name());
            self.backup_bucket = Some(Arc::new(opened));
        }
        Ok(())
    }

    /// Operator-initiated start. Clears the crash history, because a human
    /// deciding to start a server is the signal that whatever was wrong has
    /// been dealt with; that is also the only way out of a bootloop.
    pub async fn start(&self, name: &str) -> Result<(), ServerError> {
        let result = self.start_inner(name).await;
        if result.is_ok() {
            self.entry(name).await?.crashes.lock().await.reset();
            self.tell(crate::notify::Level::Info, name, "Server started")
                .await;
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
            // Worth an alert: a server that will not shut down cleanly is
            // usually a server that is about to lose the last few minutes
            // of whatever it had in memory.
            self.tell(
                crate::notify::Level::Warn,
                name,
                &format!(
                    "Did not stop within {}s, killed the session",
                    self.stop_timeout.as_secs()
                ),
            )
            .await;
        }
        *entry.state.write().await = ServerState::Stopped;
        tracing::info!("stopped {name}");
        self.tell(crate::notify::Level::Info, name, "Server stopped")
            .await;
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

    /// Restarts, telling whoever is connected first.
    ///
    /// The warning is one console line and a wait, which is all rcmd needs
    /// to know about it. Point `warn_command` at a plugin that runs its own
    /// countdown and the wait is what gives that countdown room to happen.
    ///
    /// Returns the minutes actually warned for, so the caller can say so.
    pub async fn restart_with_warning(
        &self,
        name: &str,
        update_template: bool,
        warn_minutes: Option<u64>,
    ) -> Result<u64, ServerError> {
        let seconds = match warn_minutes {
            Some(minutes) => minutes.saturating_mul(60),
            None => self.warn_seconds,
        };
        if seconds == 0 || self.warn_command.trim().is_empty() {
            self.restart(name, update_template).await?;
            return Ok(0);
        }

        // A warning nobody can receive is not worth waiting out.
        if !self.tmux.session_exists(name).await? {
            self.restart(name, update_template).await?;
            return Ok(0);
        }

        let minutes = seconds.div_ceil(60);
        let line = self
            .warn_command
            .replace("{minutes}", &minutes.to_string())
            .replace("{seconds}", &seconds.to_string())
            .replace("{reason}", "scheduled restart");
        self.tmux.session_run_command(name, &line).await?;
        tracing::info!("warned {name}, restarting in {seconds}s");
        tokio::time::sleep(Duration::from_secs(seconds)).await;

        // The warning command may well have stopped the server itself,
        // which is exactly what RedCraft's does. Restart copes with that
        // because stopping something already stopped is not an error here.
        self.restart(name, update_template).await?;
        Ok(minutes)
    }

    /// Starts everything that is not already running.
    ///
    /// Only ever called at startup, and only when the config asks for it.
    /// A server that is already up is left alone rather than restarted,
    /// which is the difference between this and a fleet restart.
    pub async fn start_all_stopped(&self) -> FleetReport {
        let names: Vec<String> = self.servers.read().await.keys().cloned().collect();
        let mut report = FleetReport::default();
        for name in names {
            if self.frozen.contains(&name) {
                report.skipped.push(name);
                continue;
            }
            match self.tmux.session_exists(&name).await {
                Ok(true) => continue,
                // Same rule as discovery: a question tmux could not answer
                // is not a server that is down.
                Err(e) => {
                    tracing::error!("not starting {name}, tmux did not answer: {e}");
                    report.failed.push(format!("{name}: {e}"));
                    continue;
                }
                Ok(false) => {}
            }
            match self.start(&name).await {
                Ok(()) => report.restarted.push(name),
                Err(e) => {
                    tracing::error!("could not start {name}: {e}");
                    report.failed.push(format!("{name}: {e}"));
                }
            }
        }
        tracing::info!(
            "auto start done: {} started, {} failed, {} skipped",
            report.restarted.len(),
            report.failed.len(),
            report.skipped.len()
        );
        report
    }

    /// Warns the whole fleet once, waits once, then restarts it.
    ///
    /// The warning goes to every server first and the wait happens once,
    /// rather than warning and waiting per server. Doing it the other way
    /// would take eight times fifteen minutes and the last server would be
    /// restarting two hours after the message that told people it would.
    ///
    /// Templates are deliberately NOT applied here. On RedCraft the warn
    /// command is a plugin that counts down and then shuts the server down
    /// itself, which the healthcheck sees as a crash, and a crash revive
    /// pulls a fresh template. That is how updates actually land, and this
    /// keeps it that way.
    pub async fn warn_and_restart_all(&self, skip: &[String]) -> FleetReport {
        let names: Vec<String> = self
            .servers
            .read()
            .await
            .keys()
            .filter(|name| !skip.contains(name))
            .cloned()
            .collect();

        let mut report = FleetReport {
            skipped: skip.to_vec(),
            ..Default::default()
        };

        let seconds = self.warn_seconds;
        if seconds > 0 && !self.warn_command.trim().is_empty() {
            let minutes = seconds.div_ceil(60);
            let line = self
                .warn_command
                .replace("{minutes}", &minutes.to_string())
                .replace("{seconds}", &seconds.to_string())
                .replace("{reason}", "Automated daily updates and restart");
            for name in &names {
                // A server that is already down needs no warning, and
                // sending one to a dead session is just an error in the log.
                if !self.tmux.session_exists(name).await.unwrap_or(false) {
                    continue;
                }
                match self.tmux.session_run_command(name, &line).await {
                    Ok(()) => report.warned.push(name.clone()),
                    Err(e) => tracing::warn!("could not warn {name}: {e}"),
                }
            }
            tracing::info!(
                "warned {} server(s), restarting the fleet in {seconds}s",
                report.warned.len()
            );
            tokio::time::sleep(Duration::from_secs(seconds)).await;
        }

        for name in &names {
            match self.restart(name, false).await {
                Ok(()) => report.restarted.push(name.clone()),
                Err(e) => {
                    tracing::error!("could not restart {name}: {e}");
                    report.failed.push(format!("{name}: {e}"));
                }
            }
        }
        tracing::info!(
            "fleet restart done: {} restarted, {} failed, {} skipped",
            report.restarted.len(),
            report.failed.len(),
            report.skipped.len()
        );
        report
    }

    /// Backs up every server, one at a time.
    ///
    /// Sequentially on purpose: eight simultaneous tar and gzip runs over
    /// the same disk is slower than doing them in order, and it competes
    /// with the servers for io while people are playing.
    pub async fn backup_all(&self) -> FleetReport {
        let names: Vec<String> = self.servers.read().await.keys().cloned().collect();
        let mut report = FleetReport::default();
        for name in names {
            match self.backup(&name).await {
                Ok(_) => report.backed_up.push(name),
                Err(e) => {
                    tracing::error!("could not back up {name}: {e}");
                    report.failed.push(format!("{name}: {e}"));
                }
            }
        }
        tracing::info!(
            "fleet backup done: {} succeeded, {} failed",
            report.backed_up.len(),
            report.failed.len()
        );
        report
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
        let (staging, upload) = match &self.backup_destination {
            BackupDestination::LocalDir { path } => (path.clone(), None),
            BackupDestination::S3 {
                key_template,
                staging_directory,
                ..
            } => (staging_directory.clone(), Some(key_template.to_string())),
        };

        let dir = entry.dir.clone();
        let dest = staging.clone();
        let mut report = tokio::task::spawn_blocking(move || {
            backup::backup_server(
                &dir.file_name().unwrap().to_string_lossy(),
                &dir,
                &config.directories_to_backup,
                &dest,
            )
        })
        .await
        .map_err(|e| ServerError::Config(format!("backup task panicked: {e}")))??;

        if let Some(key_template) = upload {
            let Some(bucket) = self.backup_bucket.as_ref() else {
                return Err(ServerError::Config(
                    "the backup bucket is configured but was never opened".into(),
                ));
            };
            let key = crate::s3::backup_key(&key_template, name, chrono::Utc::now());
            let local = std::path::PathBuf::from(&report.archive);
            bucket
                .put_file(&key, &local)
                .await
                .map_err(|e| ServerError::Config(e.to_string()))?;
            tracing::info!("uploaded {name} backup to {}/{key}", bucket.name());

            // The staging copy has served its purpose. Leaving them around
            // fills the disk with the one thing you cannot afford to have
            // fill the disk.
            let _ = std::fs::remove_file(&local);
            report.archive = format!("s3://{}/{key}", bucket.name());
            report.uploaded = true;
        }

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

            if self.frozen.contains(&name) {
                // Say it once, then stop believing it is running, so this
                // does not repeat every second forever.
                tracing::warn!("{name} is frozen and its session is gone, leaving it down");
                *entry.state.write().await = ServerState::Stopped;
                self.tell(
                    crate::notify::Level::Warn,
                    &name,
                    "Session is gone, but this server is frozen so rcmd is leaving it alone",
                )
                .await;
                continue;
            }

            let decision = entry.crashes.lock().await.record_crash(Instant::now());
            match decision {
                CrashDecision::GiveUp => {
                    tracing::error!("server crash bootloop detected for {name}, giving up");
                    *entry.state.write().await = ServerState::Crashed { bootloop: true };
                    self.tell(
                        crate::notify::Level::Fatal,
                        &name,
                        "Server crash bootloop detected, leaving it down",
                    )
                    .await;
                }
                CrashDecision::Revive { attempt } => {
                    tracing::warn!("{name} is stopped, restarting (attempt {attempt})");
                    *entry.state.write().await = ServerState::Crashed { bootloop: false };
                    self.tell(
                        crate::notify::Level::Severe,
                        &name,
                        &format!("Server is stopped, restarting (attempt {attempt})"),
                    )
                    .await;
                    if let Err(e) = self.update_template(&name, &entry.dir).await {
                        tracing::warn!("template refresh during revive of {name} failed: {e}");
                        self.tell(
                            crate::notify::Level::Warn,
                            &name,
                            &format!("Could not refresh the template before reviving: {e}"),
                        )
                        .await;
                    }
                    if let Err(e) = self.start_inner(&name).await {
                        tracing::error!("revive of {name} failed: {e}");
                        self.tell(
                            crate::notify::Level::Fatal,
                            &name,
                            &format!("Revive failed: {e}"),
                        )
                        .await;
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

    fn config_with(frozen: Vec<&str>, dir: &std::path::Path) -> Config {
        Config {
            servers_directory: dir.to_path_buf(),
            tmux_session_prefix: "rcmdtest_".into(),
            frozen_servers: frozen.into_iter().map(String::from).collect(),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn a_tmux_error_never_makes_rcmd_write_to_a_server() {
        // The dangerous shape: rcmd cannot see the sessions (wrong user,
        // private /tmp), concludes every server is stopped, and applies a
        // template, which deletes and rewrites directories under eight
        // running JVMs. An unanswerable question must mean "hands off".
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("hub");
        std::fs::create_dir_all(server.join("plugins")).unwrap();
        std::fs::write(server.join("plugins/Keep.jar"), b"precious").unwrap();

        // A template that would wipe plugins/ if it were ever applied.
        let templates = dir.path().join("templates");
        std::fs::create_dir_all(&templates).unwrap();
        let tar_path = templates.join("hub.tar");
        {
            let file = std::fs::File::create(&tar_path).unwrap();
            let mut builder = tar::Builder::new(file);
            let mut header = tar::Header::new_gnu();
            let body = b"replaced";
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, "plugins/Other.jar", &body[..])
                .unwrap();
            builder.finish().unwrap();
        }

        let mut config = config_with(vec![], dir.path());
        config.template_source = TemplateSource::LocalDir { path: templates };
        // A prefix nothing will ever match is not the failure mode here;
        // point tmux at a binary that does not exist so the query errors.
        let mut manager = Manager::new(&config);
        manager.tmux = Tmux::with_binary("rcmd-no-such-tmux".into(), "rcmdtest_".into());

        manager.discover().await.unwrap();

        // The jar is still there, and nothing from the template arrived.
        assert!(server.join("plugins/Keep.jar").exists());
        assert!(!server.join("plugins/Other.jar").exists());
    }

    #[tokio::test]
    async fn a_frozen_server_is_not_revived_by_the_healthcheck() {
        // The policy layer refuses a restart of a frozen server when a
        // person asks, but the healthcheck never goes near the policy
        // layer, so without this the automatic path quietly does the thing
        // freezing was supposed to prevent.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crea_engineer_test")).unwrap();
        let config = config_with(vec!["crea_engineer_test"], dir.path());
        let manager = Manager::new(&config);
        manager.discover().await.unwrap();

        let entry = manager.entry("crea_engineer_test").await.unwrap();
        *entry.state.write().await = ServerState::Running;

        // No session exists, so this looks exactly like a crash.
        manager.healthcheck_sweep().await;

        // Left down rather than revived, and no crash recorded against it.
        assert_eq!(*entry.state.read().await, ServerState::Stopped);
        assert_eq!(entry.crashes.lock().await.tries(), 0);
    }

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
