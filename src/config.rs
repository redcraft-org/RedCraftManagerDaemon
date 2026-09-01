//! Process configuration.
//!
//! One TOML file, environment overrides on top. This replaces rcsm's thirty
//! package-level globals read from an env file: a struct can be handed to two
//! instances in a test, reloaded, or printed, and none of that is possible
//! with process-wide state.
//!
//! The defaults are the newbie path: a servers directory next to the config,
//! SQLite, local storage, no S3, no Discord. RedCraft's deployment overrides
//! most of them.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Where the server directories live. Every directory here is a server.
    pub servers_directory: PathBuf,

    /// Prefix for tmux session names. Stays "rcsm_" by default on purpose:
    /// operator muscle memory and the cutover's session adoption both depend
    /// on the running sessions keeping their names.
    pub tmux_session_prefix: String,

    /// Seconds to wait for a clean stop before the session is killed.
    pub stop_timeout_secs: u64,

    /// Crash revives allowed inside `crash_window_secs` before the server is
    /// declared bootlooping and left down.
    pub crash_max_tries: u32,
    pub crash_window_secs: u64,

    /// Where template tars come from. Local directory by default; S3 is the
    /// power option and arrives with the artifact pipeline.
    pub template_source: TemplateSource,

    /// Where backups go.
    pub backup_destination: BackupDestination,

    /// HTTP listener.
    pub listen: String,

    /// Database. SQLite file by default; a mysql:// URL is the power option.
    pub database_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            servers_directory: PathBuf::from("/mnt/minecraft/servers"),
            tmux_session_prefix: "rcsm_".into(),
            stop_timeout_secs: 120,
            crash_max_tries: 3,
            crash_window_secs: 120,
            template_source: TemplateSource::None,
            backup_destination: BackupDestination::LocalDir {
                path: PathBuf::from("/var/lib/rcmd/backups"),
            },
            listen: "127.0.0.1:8484".into(),
            database_url: "sqlite:///var/lib/rcmd/rcmd.db".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TemplateSource {
    /// No templating: rcmd only supervises what is on disk.
    None,
    /// Tars in a local directory, named `<server>.tar`.
    LocalDir { path: PathBuf },
    // S3 { .. } lands with the artifact pipeline (M2/M4).
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupDestination {
    LocalDir { path: PathBuf },
    // S3 { .. } later.
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("could not read {}: {e}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("could not parse {}: {e}", path.display()))?;
        Ok(config)
    }

    /// The config to run with when no file exists yet: defaults, overridden
    /// by `RCMD_*` environment variables for the handful of settings a
    /// container or a test wants to set without writing a file.
    pub fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            Self::load(path)?
        } else {
            tracing::warn!(
                "no config at {}, running with defaults (rcmd init writes one)",
                path.display()
            );
            Self::default()
        };

        if let Ok(dir) = std::env::var("RCMD_SERVERS_DIRECTORY") {
            config.servers_directory = PathBuf::from(dir);
        }
        if let Ok(listen) = std::env::var("RCMD_LISTEN") {
            config.listen = listen;
        }
        if let Ok(url) = std::env::var("RCMD_DATABASE_URL") {
            config.database_url = url;
        }
        Ok(config)
    }
}

/// Per-server configuration, read from `<server_dir>/rcsm_config.json`.
///
/// The name and shape are rcsm's, kept byte-compatible on purpose: every
/// template already ships one, and the cutover depends on zero migration.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ServerConfig {
    pub start_command: String,
    pub stop_command: String,
    #[serde(default)]
    pub directories_to_backup: Vec<String>,
}

impl ServerConfig {
    pub const FILE_NAME: &'static str = "rcsm_config.json";

    pub fn read(server_dir: &Path) -> anyhow::Result<Self> {
        let path = server_dir.join(Self::FILE_NAME);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("could not read {}: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("could not parse {}: {e}", path.display()))
    }

    /// The default rcsm writes for a server directory that has none
    /// (setup.go initConfig): Aikar flags, a plain stop. Port kept so a
    /// freshly created server directory still becomes startable.
    pub fn default_for_new_server() -> Self {
        Self {
            start_command: "java -Xms6G -Xmx6G -XX:+UseG1GC -XX:+ParallelRefProcEnabled \
                            -XX:MaxGCPauseMillis=200 -XX:+UnlockExperimentalVMOptions \
                            -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -XX:G1NewSizePercent=30 \
                            -XX:G1MaxNewSizePercent=40 -XX:G1HeapRegionSize=8M \
                            -XX:G1ReservePercent=20 -XX:G1HeapWastePercent=5 \
                            -XX:G1MixedGCCountTarget=4 -XX:InitiatingHeapOccupancyPercent=15 \
                            -XX:G1MixedGCLiveThresholdPercent=90 -XX:G1RSetUpdatingPauseTimePercent=5 \
                            -XX:SurvivorRatio=32 -XX:+PerfDisableSharedMem -XX:MaxTenuringThreshold=1 \
                            -Dusing.aikars.flags=https://mcflags.emc.gs -Daikars.new.flags=true \
                            -jar server.jar nogui"
                .into(),
            stop_command: "stop".into(),
            directories_to_backup: vec![],
        }
    }

    /// Reads the config, writing the default first if the file is missing.
    pub fn read_or_init(server_dir: &Path) -> anyhow::Result<Self> {
        let path = server_dir.join(Self::FILE_NAME);
        if !path.exists() {
            let default = Self::default_for_new_server();
            std::fs::write(&path, serde_json::to_string_pretty(&default)?)?;
            tracing::info!("wrote default {} for {}", Self::FILE_NAME, server_dir.display());
            return Ok(default);
        }
        Self::read(server_dir)
    }
}
