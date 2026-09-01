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

    /// Where the catalog and templates are read from. This is meant to be
    /// a git checkout, so the whole configuration is reviewable.
    pub config_directory: PathBuf,

    /// Where fetched jars are kept.
    pub artifact_directory: PathBuf,

    /// Where built template tars are written. Point `template_source` at
    /// the same place to have the servers pick them up.
    pub build_directory: PathBuf,

    /// Cron expression for the nightly refresh and rebuild. Empty disables
    /// it, which is what you want when a CI job drives the pipeline
    /// instead.
    pub pipeline_schedule: String,

    /// Where to look for a server's log, relative to its directory.
    ///
    /// Tried in order, first pattern that matches anything wins, and `*` is
    /// the only wildcard. When several files match one pattern the most
    /// recently written one is used, which is what you want when something
    /// rotates its logs.
    ///
    /// Nothing matching is fine: rcmd falls back to the tmux pane, which is
    /// all a non Minecraft process usually has anyway.
    pub log_files: Vec<String>,

    /// Per server overrides for the above, for the one process that keeps
    /// its log somewhere nobody would guess.
    pub log_files_by_server: std::collections::BTreeMap<String, Vec<String>>,

    /// Start every stopped server when rcmd starts.
    ///
    /// Off by default, because a tool that starts servers you did not ask
    /// it to start is a surprise. Turn it on and a reboot of the box brings
    /// the fleet back on its own.
    ///
    /// Please notice that a server left down as a bootloop stays down: it
    /// is started once here, and if it dies again the usual crash counting
    /// takes over.
    pub auto_start_on_boot: bool,

    /// When to back every server up. Empty disables it.
    pub backup_schedule: String,

    /// When to warn everybody and restart the whole fleet. Empty disables
    /// it, and that is the default, because a tool that restarts your
    /// servers on a schedule you did not ask for is not a good tool.
    pub restart_schedule: String,

    /// Servers nothing may restart, whoever asks. Keep the one server
    /// somebody has spent a year setting up by hand in here.
    pub frozen_servers: Vec<String>,

    /// Servers that are not Minecraft. They still start and stop, they just
    /// have no template and no players to warn.
    pub non_minecraft_servers: Vec<String>,

    /// Plugins, the things that tell rcmd about the rest of your setup.
    ///
    /// Configure one that reports players and a restart will refuse to drop
    /// them. Without one rcmd does not ask, and does not pretend to know.
    pub plugins: Vec<crate::plugins::PluginConfig>,

    /// What to send to a server before restarting it, and how long to wait
    /// afterwards. `{minutes}` and `{reason}` are filled in.
    ///
    /// By default this is a plain `say`, because that works on every server
    /// ever made. Please notice that if you run something that does its own
    /// countdown and shutdown, point this at that instead.
    pub warn_command: String,
    pub warn_seconds: u64,

    /// When pushes to public repositories are allowed. Unset means always.
    pub push_window: Option<PushWindow>,

    /// Discord webhook for pipeline results and server crashes. Unset means
    /// no notifications.
    pub discord_webhook_url: Option<String>,

    /// What to call this rcmd in a notification, so two of them posting into
    /// the same channel can be told apart.
    pub instance_name: String,

    /// The branch rcmd follows in the config repository, when the config
    /// directory is a git checkout.
    pub config_branch: String,
}

/// The hours during which pushing to a public repository is allowed.
///
/// This exists because a repository full of commits timestamped 03:00 on a
/// Tuesday tells everybody watching that a robot is driving, and the point
/// of reviewing a config repository is that a person is.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PushWindow {
    /// Hours to add to UTC to get local time. Quebec is -4 in summer and
    /// -5 in winter, and rcmd does not carry a timezone database to work
    /// that out for you.
    pub utc_offset_hours: i32,
    /// The window opens at this hour, local.
    pub from_hour: u32,
    /// And closes at this one. Set it past midnight and it wraps.
    pub to_hour: u32,
    /// Weekends are outside the window when this is on.
    pub weekdays_only: bool,
}

impl Default for PushWindow {
    fn default() -> Self {
        Self {
            utc_offset_hours: -4,
            from_hour: 17,
            to_hour: 24,
            weekdays_only: true,
        }
    }
}

impl PushWindow {
    /// Whether the window is open at a given moment.
    pub fn is_open_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        use chrono::{Datelike, Timelike};
        let local = now + chrono::Duration::hours(self.utc_offset_hours as i64);
        if self.weekdays_only && local.weekday().number_from_monday() > 5 {
            return false;
        }
        let hour = local.hour();
        if self.from_hour <= self.to_hour {
            hour >= self.from_hour && hour < self.to_hour
        } else {
            // A window that wraps past midnight, like 22 to 6.
            hour >= self.from_hour || hour < self.to_hour
        }
    }
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
            config_directory: PathBuf::from("/var/lib/rcmd/config"),
            artifact_directory: PathBuf::from("/var/lib/rcmd/artifacts"),
            build_directory: PathBuf::from("/var/lib/rcmd/builds"),
            pipeline_schedule: "0 3 * * *".into(),
            log_files: vec!["logs/latest.log".into(), "logs/*.log".into()],
            log_files_by_server: Default::default(),
            auto_start_on_boot: false,
            backup_schedule: String::new(),
            restart_schedule: String::new(),
            frozen_servers: vec![],
            non_minecraft_servers: vec![],
            plugins: vec![],
            warn_command: "say Restarting in {minutes} minute(s): {reason}".into(),
            warn_seconds: 0,
            push_window: None,
            discord_webhook_url: None,
            instance_name: "rcmd".into(),
            config_branch: "main".into(),
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
    /// Tars in a bucket, under the same `<server>.tar` key the old daemon
    /// used. Keeping the key identical is what makes a rollback possible.
    S3 {
        #[serde(flatten)]
        bucket: crate::s3::BucketConfig,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupDestination {
    LocalDir {
        path: PathBuf,
    },
    S3 {
        #[serde(flatten)]
        bucket: crate::s3::BucketConfig,
        /// Where the archive goes. `{server}`, `{date}` and `{datetime}`
        /// are filled in.
        ///
        /// :warning: :warning: :warning: The default overwrites the same key
        /// every single run, which is what the old daemon did, so there is
        /// only ever ONE backup per server and backing up a broken world
        /// DESTROYS the good copy. Put `{date}` in here if you want
        /// history, and keep an eye on what that costs you.
        #[serde(default = "default_backup_key")]
        key_template: String,
        /// Where the archive is built before it goes up.
        #[serde(default = "default_staging")]
        staging_directory: PathBuf,
    },
}

fn default_backup_key() -> String {
    "{server}.tar.gz".into()
}

fn default_staging() -> PathBuf {
    PathBuf::from("/var/lib/rcmd/backup-staging")
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
        if let Ok(dir) = std::env::var("RCMD_CONFIG_DIRECTORY") {
            config.config_directory = PathBuf::from(dir);
        }
        // A webhook url is a credential: anybody holding it can post into
        // your channel. This lets it live in the environment file with the
        // other secrets rather than in a config file people read over your
        // shoulder.
        if let Ok(url) = std::env::var("RCMD_DISCORD_WEBHOOK_URL") {
            config.discord_webhook_url = Some(url);
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
            tracing::info!(
                "wrote default {} for {}",
                Self::FILE_NAME,
                server_dir.display()
            );
            return Ok(default);
        }
        Self::read(server_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(day: u32, hour: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 9, day, hour, 0, 0)
            .unwrap()
    }

    #[test]
    fn the_push_window_opens_at_five_local_on_a_weekday() {
        let window = PushWindow::default();
        // 2026-09-01 is a Tuesday. 20:00 UTC is 16:00 in Quebec, still shut.
        assert!(!window.is_open_at(utc(1, 20)));
        // 21:00 UTC is 17:00 local, open.
        assert!(window.is_open_at(utc(1, 21)));
        // 2026-09-05 is a Saturday, so the hour does not matter.
        assert!(!window.is_open_at(utc(5, 22)));
    }

    #[test]
    fn a_window_may_wrap_past_midnight() {
        let window = PushWindow {
            from_hour: 22,
            to_hour: 6,
            weekdays_only: false,
            utc_offset_hours: 0,
        };
        assert!(window.is_open_at(utc(1, 23)));
        assert!(window.is_open_at(utc(1, 3)));
        assert!(!window.is_open_at(utc(1, 12)));
    }

    #[test]
    fn the_defaults_parse_from_an_empty_file() {
        // Every key optional is the promise the config makes, and an empty
        // file is the strongest test of it.
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.tmux_session_prefix, "rcsm_");
        assert_eq!(config.crash_max_tries, 3);
        assert!(config.push_window.is_none());
    }

    #[test]
    fn an_unknown_key_is_a_typo_and_says_so() {
        // Silently ignoring frozen_server (no s) would mean a server you
        // thought was protected is not.
        let error = toml::from_str::<Config>("frozen_server = [\"hub\"]").unwrap_err();
        assert!(error.to_string().contains("frozen_server"));
    }
}
