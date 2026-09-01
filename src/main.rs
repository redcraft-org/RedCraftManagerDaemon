//! rcmd — RedCraftManagerDaemon.
//!
//! One service that builds, ships and runs a Minecraft server fleet:
//! plugin catalog, layered templates, deployment, process supervision, and
//! the API/web/MCP surfaces over all of it.

mod api;
mod config;
mod domain;
mod servers;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rcmd", version, about = "RedCraftManagerDaemon")]
struct Cli {
    /// Path to config.toml.
    #[arg(long, default_value = "/opt/rcmd/config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Run the daemon (the default when no subcommand is given).
    Serve,
    /// Interactive first-run setup: writes config.toml.
    Init,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rcmd=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(CliCommand::Serve) {
        CliCommand::Serve => serve(&cli.config),
        CliCommand::Init => init(&cli.config),
    }
}

fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = config::Config::load_or_default(config_path)?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let manager = Arc::new(servers::Manager::new(&config));
        let discovered = manager.discover().await?;
        tracing::info!("supervising {} servers: {}", discovered.len(), discovered.join(", "));

        let _health = manager.spawn_healthcheck();

        let state = Arc::new(api::AppState {
            manager,
            started_at: chrono::Utc::now(),
        });
        let app = api::router(state);

        let listener = tokio::net::TcpListener::bind(&config.listen).await?;
        tracing::info!("rcmd listening on {}", config.listen);
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("shutting down (servers keep running in tmux)");
            })
            .await?;
        Ok::<_, anyhow::Error>(())
    })
}

fn init(config_path: &std::path::Path) -> anyhow::Result<()> {
    if config_path.exists() {
        anyhow::bail!("{} already exists, refusing to overwrite", config_path.display());
    }

    // Minimal wizard for now: sensible defaults, the one question that has
    // no default answer. Grows alongside the features.
    eprint!("Servers directory [/mnt/minecraft/servers]: ");
    let mut dir = String::new();
    std::io::stdin().read_line(&mut dir)?;
    let dir = dir.trim();
    let servers_directory = if dir.is_empty() {
        "/mnt/minecraft/servers".to_string()
    } else {
        dir.to_string()
    };

    let contents = format!(
        r#"# rcmd configuration. Every key is optional; these are the defaults
# with your answers filled in.

servers_directory = "{servers_directory}"
tmux_session_prefix = "rcsm_"
stop_timeout_secs = 120
crash_max_tries = 3
crash_window_secs = 120
listen = "127.0.0.1:8484"
database_url = "sqlite:///var/lib/rcmd/rcmd.db"

[template_source]
kind = "none"

[backup_destination]
kind = "local_dir"
path = "/var/lib/rcmd/backups"
"#
    );
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(config_path, contents)?;
    eprintln!("wrote {}", config_path.display());
    Ok(())
}
