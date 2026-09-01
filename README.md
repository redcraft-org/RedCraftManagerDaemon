# RedCraft Manager Daemon (rcmd)

This Rust project runs a fleet of Minecraft servers: it downloads the plugins, builds the server templates, applies them and keeps the servers alive, with a REST API, a web interface and an MCP endpoint on top.

It replaces three tools that used to talk to each other through cron timings and a fire and forget Redis channel:

- `redcraft_plugins_updater` downloaded the plugin jars into a bucket
- `redcraft_server_templates` built the server tars out of layered configs
- `redcraft_server_management` ran the servers and applied those tars

Doing it in one process means the ordering is no longer a matter of hoping the 3am job finished before the 4am one.

## Requirements

- `tmux`, because every server runs in a detached session
- Java, or whatever your servers actually need to start
- A database, but only if you want one. By default rcmd uses a SQLite file it creates itself

rcmd is meant to be easy on a single server and configurable on a big one. Every external dependency is optional and has a local default, so you can start with nothing and turn things on later.

## How to install

- Download the latest release from the [GitHub releases](https://github.com/redcraft-org/RedCraftManagerDaemon/releases) as `rcmd`
- Set it as runnable with `chmod +x rcmd`
- Run `./rcmd init` to write a `config.toml`
- Start it with `./rcmd --config config.toml serve`

The web interface is on `http://127.0.0.1:8484` by default.

:warning: There is no authentication yet. Keep the bind on `127.0.0.1` and put something like nginx or a VPN in front of it, because anything that can reach the port can restart your servers.

## Config

Every key in `config.toml` is optional, these are the defaults:

- `servers_directory` is where your servers live, by default `/mnt/minecraft/servers`. Every directory in there is a server
- `tmux_session_prefix` is what rcmd names its tmux sessions, by default `rcsm_` so existing sessions keep working
- `stop_timeout_secs` is how long to wait for a clean stop before killing the session, by default 120
- `crash_max_tries` and `crash_window_secs` control the bootloop detection, by default 3 crashes in 120 seconds
- `listen` is the HTTP bind, by default `127.0.0.1:8484`
- `template_source` is where template tars come from, by default `none` which means rcmd only supervises what is already on disk
- `backup_destination` is where backups go, by default a local directory

Each server also needs a `rcsm_config.json` in its own directory. If it's missing, rcmd writes one with Aikar's flags so at least the server can start:

- `start_command` is the command that starts the server, and it's split on spaces, so don't expect shell quoting to work
- `stop_command` is the command to gracefully stop the server, by default it's `stop` but for BungeeCord or Velocity you'll have to set it to `end` or `shutdown`
- `directories_to_backup` is the list of directories to include in a backup, relative to the server directory

## How it works

### Servers run in tmux

Every server gets a detached tmux session named after it. That means rcmd can be restarted, upgraded or crash without touching a single running server, and it will adopt whatever sessions it finds when it comes back. You can still `tmux attach -t rcsm_hub` and type in the console yourself.

Please notice that rcmd targets sessions exactly. tmux normally matches on prefixes, so a command aimed at `rcsm_crea` would land in `rcsm_crea_build_plot` if that's the only session starting with that name. That's a fun way to stop the wrong server, so rcmd doesn't do it.

### Applying a template

A template is a tar. Applying it removes and rewrites every path the tar names, and leaves everything else alone.

Basically, if `plugins/` is in the tar then your whole `plugins/` directory is deleted and rebuilt from the template. Your worlds, logs and caches aren't in the tar, so they survive.

:warning: :warning: :warning: If you store data inside a directory that the template also ships, applying a template will DELETE it! Plugin data directories under `plugins/` are the usual victim.

Template tars are also never trusted with paths. An entry trying to write outside the server directory is refused instead of applied, because "the bucket is ours" isn't a security model.

### Crashes and restarts

rcmd checks every second whether a server it believes is running still has a session. If it doesn't, that's a crash, and rcmd applies a fresh template before starting it again. This is on purpose: a crash is exactly the moment you want the server put back the way the template says it should be.

An explicit restart doesn't do that, unless you ask for it. Restarting from the web interface gives you both buttons.

By default, a server that crashes more than 3 times in 120 seconds is left down and marked as a bootloop, because reviving it a fourth time won't fix it either. Starting it yourself clears that.

### Console output

Anything a player can type ends up in the console, so the API marks console responses as untrusted and the web interface shows a banner. Keep that in mind if you ever pipe this into something that reads instructions, like an LLM (yes, that's the point).

## API

The web interface is just a client of this, it has no privileged path of its own.

- `GET /api/v1/servers` lists every server with its state
- `GET /api/v1/servers/<server_name>` for a single one
- `POST /api/v1/servers/<server_name>/start`, `/stop`, `/restart`, `/backup`
- `POST /api/v1/servers/<server_name>/command` with a JSON body like `{"command": "say hello"}`
- `GET /api/v1/servers/<server_name>/console?lines=100`

`restart` takes `{"update_template": true}` or `false`, and defaults to true.

## Contributing

You are free to suggest changes by opening an issue ticket.

You can also open PRs, remember to bump the version in `Cargo.toml` before opening a pull request.
