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
mod install;
mod mcp;
mod notify;
mod pipeline;
mod plugins;
mod policy;
mod s3;
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
    /// Install the systemd service and put the binary in place.
    ///
    /// The binary is its own installer, so getting rcmd running is
    /// downloading one file and running this once.
    Install {
        /// Who the service runs as. Defaults to whoever owns the servers
        /// directory, which is the user whose tmux sessions the servers are
        /// in. Running as anybody else means rcmd cannot see them.
        #[arg(long)]
        user: Option<String>,
        /// Where the binary should live.
        #[arg(long, default_value = "/usr/local/bin/rcmd")]
        binary: PathBuf,
        /// Enable and start it straight away.
        #[arg(long)]
        start: bool,
    },
    /// Stop the service and remove the unit.
    ///
    /// Your servers keep running: they are in tmux, not in the service.
    Uninstall {
        /// Also delete /etc/rcmd, /var/lib/rcmd and the binary.
        #[arg(long)]
        purge: bool,
        /// Where the binary lives, for --purge.
        #[arg(long, default_value = "/usr/local/bin/rcmd")]
        binary: PathBuf,
    },
    /// Check that rcmd can reach everything it is configured to use.
    ///
    /// Read only. Worth running before you point it at a live fleet,
    /// because the alternative is finding out that the bucket credentials
    /// are wrong at the moment a server crashes and needs its template.
    Doctor,
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
        CliCommand::Install {
            user,
            binary,
            start,
        } => {
            // Only the servers directory is needed, and a missing config is
            // the normal first run, so defaults are fine here.
            let servers = config::Config::load_or_default(&cli.config)
                .map(|c| c.servers_directory)
                .unwrap_or_else(|_| PathBuf::from("/mnt/minecraft/servers"));
            install::install(&cli.config, &servers, user, &binary, start).map_err(Into::into)
        }
        CliCommand::Uninstall { purge, binary } => {
            install::uninstall(purge, &binary).map_err(Into::into)
        }
        CliCommand::Doctor => doctor(&cli.config),
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

fn doctor(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = config::Config::load_or_default(config_path)?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let mut problems = 0;

        println!("Servers directory {}", config.servers_directory.display());
        let servers: Vec<String> = match std::fs::read_dir(&config.servers_directory) {
            Ok(read) => read
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(e) => {
                println!("  cannot read it: {e}");
                problems += 1;
                vec![]
            }
        };
        println!("  {} server(s): {}", servers.len(), servers.join(", "));

        // tmux is how rcmd sees anything at all. If this is wrong nothing
        // else matters, and the failure is silent and destructive.
        let tmux = servers::tmux::Tmux::new(config.tmux_session_prefix.clone());
        let mut alive = 0;
        for name in &servers {
            match tmux.session_exists(name).await {
                Ok(true) => alive += 1,
                Ok(false) => {}
                Err(e) => {
                    println!("  tmux did not answer about {name}: {e}");
                    problems += 1;
                }
            }
        }
        println!("  {alive} of them have a live tmux session");

        match &config.template_source {
            config::TemplateSource::None => println!("Templates: none configured"),
            config::TemplateSource::LocalDir { path } => {
                println!("Templates from {}", path.display());
                for name in &servers {
                    let tar = path.join(format!("{name}.tar"));
                    match std::fs::metadata(&tar) {
                        Ok(m) => println!("  {name}: {} bytes", m.len()),
                        Err(_) => println!("  {name}: no template"),
                    }
                }
            }
            config::TemplateSource::S3 { bucket } => {
                println!("Templates from s3://{}", bucket.bucket);
                match s3::Bucket::connect(bucket).await {
                    Err(e) => {
                        println!("  cannot connect: {e}");
                        problems += 1;
                    }
                    Ok(opened) => {
                        for name in &servers {
                            let key = s3::template_key(name);
                            match opened.get(&key).await {
                                Ok(bytes) => println!("  {name}: {} bytes", bytes.len()),
                                Err(s3::S3Error::Missing { .. }) => {
                                    println!("  {name}: no template in the bucket")
                                }
                                Err(e) => {
                                    println!("  {name}: {e}");
                                    problems += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        match &config.backup_destination {
            config::BackupDestination::LocalDir { path } => {
                println!("Backups to {}", path.display())
            }
            config::BackupDestination::S3 {
                bucket,
                key_template,
                ..
            } => {
                println!("Backups to s3://{} as {key_template}", bucket.bucket);
                match s3::Bucket::connect(bucket).await {
                    Ok(_) => println!("  credentials accepted"),
                    Err(e) => {
                        println!("  cannot connect: {e}");
                        problems += 1;
                    }
                }
            }
        }

        match store::Database::connect(&config.database_url).await {
            Ok(_) => println!("Database {} is reachable", config.database_url),
            Err(e) => {
                println!("Database {}: {e}", config.database_url);
                problems += 1;
            }
        }

        for (what, expression) in [
            ("pipeline", &config.pipeline_schedule),
            ("backup", &config.backup_schedule),
            ("restart", &config.restart_schedule),
        ] {
            if expression.trim().is_empty() {
                println!("No {what} schedule");
            } else if let Err(e) = scheduler::Schedule::parse(expression) {
                println!("The {what} schedule {expression:?} is not valid: {e}");
                problems += 1;
            } else {
                println!("The {what} schedule {expression:?} parses");
            }
        }

        if problems == 0 {
            println!("\nNothing looks wrong.");
            Ok(())
        } else {
            anyhow::bail!("{problems} problem(s)")
        }
    })
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

/// The token rcmd writes to the config repository with.
///
/// `RCMD_CONFIG_TOKEN` first, so it can be a fine grained token scoped to
/// that one repository. `GITHUB_TOKEN` is the fallback for the simple case
/// where one classic token does everything.
///
/// :warning: Do not put a narrowly scoped token in `GITHUB_TOKEN` alone.
/// That one is also sent to every repository the catalog fetches from, and
/// a token those repositories refuse turns every download into a failure.
fn config_repo_token() -> Option<String> {
    std::env::var("RCMD_CONFIG_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .filter(|token| !token.trim().is_empty())
}

fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = config::Config::load_or_default(config_path)?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let notifier = Arc::new(notify::Notifier::named(
            config.discord_webhook_url.clone(),
            config.instance_name.clone(),
        ));

        let mut manager = servers::Manager::new(&config);
        manager.set_notifier(notifier.clone());
        manager.open_buckets().await?;
        let manager = Arc::new(manager);
        let discovered = manager.discover().await?;
        tracing::info!(
            "supervising {} servers: {}",
            discovered.len(),
            discovered.join(", ")
        );

        if config.auto_start_on_boot {
            // Before the healthcheck, so a server that takes a while to
            // come up is not immediately counted as a crash.
            manager.start_all_stopped().await;
        }

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
            let repo = gitconfig::ConfigRepo::new(
                &config.config_directory,
                config.config_branch.clone(),
                config_repo_token(),
            );
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
            plugins: plugins::Plugins::load(&config.plugins),
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
            notifier,
            pipeline_lock: tokio::sync::Mutex::new(()),
            roster_cache: tokio::sync::RwLock::new(None),
        });

        let _schedules = scheduler::spawn_all(state.clone(), &config);

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
