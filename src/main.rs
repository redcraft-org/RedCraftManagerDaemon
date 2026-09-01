//! rcmd, RedCraftManagerDaemon.
//!
//! One service that builds, ships and runs a Minecraft server fleet:
//! plugin catalog, layered templates, deployment, process supervision, and
//! the API/web/MCP surfaces over all of it.

mod api;
mod artifacts;
mod auth;
mod catalog;
mod config;
mod domain;
mod gitconfig;
mod mcp;
mod notify;
mod pipeline;
mod policy;
mod scheduler;
mod servers;
mod store;
mod templates;
mod web;

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
    /// Check a config directory without running anything.
    ///
    /// This is what the config repository's own CI runs on a pull request,
    /// so a broken pin gets caught by a robot rather than at 3am.
    Validate {
        /// The directory to check. Defaults to the configured one.
        directory: Option<PathBuf>,
    },
    /// Mint an API token and print it once.
    Token {
        /// What to call it, so you can tell your tokens apart later.
        name: String,
        /// viewer, operator or admin.
        #[arg(long, default_value = "admin")]
        role: String,
    },
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
        CliCommand::Validate { directory } => validate(&cli.config, directory),
        CliCommand::Token { name, role } => mint_token(&cli.config, &name, &role),
    }
}

fn validate(config_path: &std::path::Path, directory: Option<PathBuf>) -> anyhow::Result<()> {
    let root = match directory {
        Some(directory) => directory,
        None => config::Config::load_or_default(config_path)?.config_directory,
    };
    println!("Checking {}", root.display());

    let (catalog, templates) = pipeline::load_config(&root)?;
    println!("  {} artifacts", catalog.artifacts.len());
    println!("  {} templates", templates.by_name.len());

    // The check that matters: a template pinning something the catalog does
    // not have builds fine right up until the moment it does not.
    let mut problems = Vec::new();
    for name in templates.by_name.keys() {
        let resolved = match templates.resolved(name) {
            Ok(resolved) => resolved,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        for artifact in resolved.artifacts.keys() {
            if catalog.get(artifact).is_none() {
                problems.push(format!(
                    "{name} pins {artifact:?}, which is not in the catalog"
                ));
            }
        }
        if resolved.buildable && resolved.artifacts.is_empty() {
            problems.push(format!("{name} is buildable but pins nothing at all"));
        }
    }

    // A secret that no config file mentions is a placeholder somebody
    // renamed in one place and not the other, and it substitutes to nothing
    // in silence.
    //
    // The search covers the whole inheritance chain, in both directions: a
    // base layer commonly declares a placeholder its children's files use,
    // and a child commonly declares one that only appears in the parent's.
    // Only buildable templates are checked, because a layer on its own is
    // not supposed to be complete.
    for name in templates.by_name.keys() {
        let (Ok(resolved), Ok(chain)) = (templates.resolved(name), templates.chain(name)) else {
            continue;
        };
        if !resolved.buildable {
            continue;
        }
        for placeholder in resolved.secrets.keys() {
            let found = chain
                .iter()
                .any(|layer| mentions(&layer.directory.join("config"), placeholder));
            if !found {
                problems.push(format!(
                    "{name} substitutes {placeholder:?}, which appears in none of its config files"
                ));
            }
        }
    }

    if problems.is_empty() {
        println!("Everything checks out.");
        return Ok(());
    }
    for problem in &problems {
        eprintln!("  {problem}");
    }
    anyhow::bail!("{} problem(s)", problems.len())
}

/// Whether any config file under a directory contains a string.
fn mentions(directory: &std::path::Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if mentions(&path, needle) {
                return true;
            }
        } else if std::fs::read_to_string(&path)
            .map(|text| text.contains(needle))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn mint_token(config_path: &std::path::Path, name: &str, role: &str) -> anyhow::Result<()> {
    let config = config::Config::load_or_default(config_path)?;
    let role = auth::Role::parse(role)
        .ok_or_else(|| anyhow::anyhow!("{role:?} is not a role: viewer, operator or admin"))?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let database = store::Database::connect(&config.database_url).await?;
        let (_, secret) = auth::Tokens::new(&database).create(name, role).await?;
        // stdout, so it pipes. Everything else this binary says goes to the
        // log, and you do not want your token in the journal.
        println!("{secret}");
        eprintln!("{role} token for {name:?}. Write it down, it is not stored.");
        Ok::<_, anyhow::Error>(())
    })
}

fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = config::Config::load_or_default(config_path)?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let manager = Arc::new(servers::Manager::new(&config));
        let discovered = manager.discover().await?;
        tracing::info!(
            "supervising {} servers: {}",
            discovered.len(),
            discovered.join(", ")
        );

        let _health = manager.spawn_healthcheck();

        let database = Arc::new(store::Database::connect(&config.database_url).await?);
        tracing::info!("database ready at {}", config.database_url);

        // A fresh database has nobody in it, and a daemon nobody can log
        // into is not useful. The first admin token is printed once, here,
        // and that is the only time it exists in readable form.
        let tokens = auth::Tokens::new(&database);
        if !tokens.any_usable().await? {
            let (_, secret) = tokens.create("first admin", auth::Role::Admin).await?;
            tracing::warn!(
                "no tokens existed, so here is an admin one. Copy it now, \
                 it is not recoverable: {secret}"
            );
        }

        // The config directory is a git checkout when somebody set one up,
        // and just a directory otherwise. Both work; only the first can
        // propose pin bumps or follow a remote.
        let config_repo = {
            let repo =
                gitconfig::ConfigRepo::new(&config.config_directory, config.config_branch.clone());
            if repo.is_checkout() {
                match repo.sync().await {
                    Ok(report) if report.changed => {
                        tracing::info!(
                            "config repository moved to {}",
                            &report.after[..7.min(report.after.len())]
                        )
                    }
                    Ok(_) => tracing::info!("config repository is up to date"),
                    Err(e) => tracing::warn!("could not sync the config repository: {e}"),
                }
                Some(repo)
            } else {
                None
            }
        };

        // A missing config directory is a first run, not a failure. The
        // daemon still supervises servers with an empty catalog, which is
        // exactly what you want while you are still writing the thing.
        let (catalog, templates) = match pipeline::load_config(&config.config_directory) {
            Ok(loaded) => loaded,
            Err(e) => {
                tracing::warn!(
                    "no usable config in {}: {e}. Starting with an empty catalog.",
                    config.config_directory.display()
                );
                (catalog::Catalog::default(), templates::Templates::default())
            }
        };
        tracing::info!(
            "{} artifacts and {} templates loaded",
            catalog.artifacts.len(),
            templates.by_name.len()
        );

        let state = Arc::new(api::AppState {
            manager,
            database: database.clone(),
            started_at: chrono::Utc::now(),
            frozen_servers: config.frozen_servers.clone(),
            non_minecraft_servers: config.non_minecraft_servers.clone(),
            player_count_url: config.player_count_url.clone(),
            push_window: config.push_window.clone(),
            config: tokio::sync::RwLock::new(api::LoadedConfig { catalog, templates }),
            manifest: database.clone(),
            store: Arc::new(artifacts::store::DirectoryStore::new(
                &config.artifact_directory,
            )),
            resolver: Arc::new(artifacts::sources::Resolver::new(
                std::env::var("GITHUB_TOKEN").ok(),
            )),
            config_directory: config.config_directory.clone(),
            build_directory: config.build_directory.clone(),
            workspace: config.build_directory.join(".work"),
            repo: config_repo,
            notifier: notify::Notifier::new(config.discord_webhook_url.clone()),
            pipeline_lock: tokio::sync::Mutex::new(()),
        });

        let _scheduler = scheduler::spawn(state.clone(), &config.pipeline_schedule);

        let app = api::router(state);

        let listener = tokio::net::TcpListener::bind(&config.listen).await?;
        tracing::info!("rcmd listening on http://{}", config.listen);
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
        anyhow::bail!(
            "{} already exists, refusing to overwrite",
            config_path.display()
        );
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

# Where the catalog and the templates are read from. Point this at a git
# checkout and your whole configuration becomes reviewable.
config_directory = "/var/lib/rcmd/config"
artifact_directory = "/var/lib/rcmd/artifacts"
build_directory = "/var/lib/rcmd/builds"

# Nightly refresh and rebuild. Empty this to run the pipeline yourself.
pipeline_schedule = "0 3 * * *"

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
