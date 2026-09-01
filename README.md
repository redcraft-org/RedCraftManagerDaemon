# RedCraft Manager Daemon (rcmd)

This Rust project runs a fleet of Minecraft servers: it downloads the plugins, builds the server templates, applies them and keeps the servers alive, with a REST API, a web interface and an MCP endpoint on top.

It replaces three tools that used to talk to each other through cron timings and a fire and forget Redis channel:

- `redcraft_plugins_updater` downloaded the plugin jars into a bucket
- `redcraft_server_templates` built the server tars out of layered configs
- `redcraft_server_management` ran the servers and applied those tars

Doing it in one process means the ordering is no longer a matter of hoping the 3am job finished before the 4am one.

## Requirements

- `tmux`, because every server runs in a detached session
- `git`, if you want to keep your configuration in a repository (you do)
- Java, or whatever your servers actually need to start
- A database, but only if you want one. By default rcmd uses a SQLite file it creates itself

rcmd is meant to be easy on a single server and configurable on a big one. Every external dependency is optional and has a local default, so you can start with nothing and turn things on later.

## How to install

- Download the latest release from the [GitHub releases](https://github.com/redcraft-org/RedCraftManagerDaemon/releases) as `rcmd`
- Set it as runnable with `chmod +x rcmd`
- Run `./rcmd init` to write a `config.toml`
- Start it with `./rcmd --config config.toml serve`

### As a service

There's a unit file in `packaging/rcmd.service`, and the release ships it too:

- Put the binary at `/usr/local/bin/rcmd` and the unit at `/etc/systemd/system/rcmd.service`
- Write your config to `/etc/rcmd/config.toml`, and any secrets to `/etc/rcmd/env` as `NAME=value` lines
- `systemctl enable --now rcmd`

Please notice that stopping rcmd doesn't stop a single server. They're in their own tmux sessions, and rcmd adopts them again when it comes back, so you can upgrade it in the middle of the afternoon.

On the first run rcmd finds no tokens and mints an admin one, printing it to the log exactly once. Copy it, that's your login. You can make more later with `./rcmd token <name> --role operator`.

The web interface is on `http://127.0.0.1:8484` by default. Paste the token and you're in.

:warning: There is no TLS. Keep the bind on `127.0.0.1` and put something like nginx or Caddy in front of it, because a token going over plain HTTP is a token somebody else has.

## Config

Every key in `config.toml` is optional, these are the defaults:

- `servers_directory` is where your servers live, by default `/mnt/minecraft/servers`. Every directory in there is a server
- `tmux_session_prefix` is what rcmd names its tmux sessions, by default `rcsm_` so existing sessions keep working
- `stop_timeout_secs` is how long to wait for a clean stop before killing the session, by default 120
- `crash_max_tries` and `crash_window_secs` control the bootloop detection, by default 3 crashes in 120 seconds
- `listen` is the HTTP bind, by default `127.0.0.1:8484`
- `database_url` is by default `sqlite:///var/lib/rcmd/rcmd.db`. A `mysql://` URL works too, if you already run one
- `config_directory` is where the catalog and the templates are read from, by default `/var/lib/rcmd/config`. Point it at a git checkout and rcmd follows the branch
- `config_branch` is the branch it follows, by default `main`
- `artifact_directory` is where downloaded jars are kept
- `build_directory` is where built template tars are written. Point `template_source` at the same place to have your servers pick them up
- `pipeline_schedule` is when to fetch and rebuild, by default `0 3 * * *`. Empty it if a CI job drives the pipeline instead
- `template_source` is where template tars come from, by default `none` which means rcmd only supervises what is already on disk
- `backup_destination` is where backups go, by default a local directory
- `frozen_servers` is a list of servers nothing may restart, whoever asks. Put the one somebody spent a year setting up by hand in here
- `non_minecraft_servers` is a list of processes that aren't Minecraft. They still start and stop, they just have no template
- `player_count_url` is where to ask how many players are connected, so a restart can refuse to drop them. Anything answering with a bare number, or JSON with `players.online` or `online`, works
- `warn_command` is what to send before a restart, by default `say Restarting in {minutes} minute(s): {reason}`. `{minutes}`, `{seconds}` and `{reason}` are filled in
- `warn_seconds` is how long to wait after warning, by default 0 which means don't warn at all
- `discord_webhook_url` gets a summary after every pipeline run. Unset means no notifications
- `push_window` limits when rcmd may push to a public repository, see below

Each server also needs a `rcsm_config.json` in its own directory. If it's missing, rcmd writes one with Aikar's flags so at least the server can start:

- `start_command` is the command that starts the server, and it's split on spaces, so don't expect shell quoting to work
- `stop_command` is the command to gracefully stop the server, by default it's `stop` but for BungeeCord or Velocity you'll have to set it to `end` or `shutdown`
- `directories_to_backup` is the list of directories to include in a backup, relative to the server directory

## How it works

### Servers run in tmux

Every server gets a detached tmux session named after it. That means rcmd can be restarted, upgraded or crash without touching a single running server, and it will adopt whatever sessions it finds when it comes back. You can still `tmux attach -t rcsm_hub` and type in the console yourself.

Please notice that rcmd targets sessions exactly. tmux normally matches on prefixes, so a command aimed at `rcsm_crea` would land in `rcsm_crea_build_plot` if that's the only session starting with that name. That's a fun way to stop the wrong server, so rcmd doesn't do it.

### Your configuration lives in git

The catalog of plugins and the server templates are YAML in a repository, not rows in a database. rcmd reads that repository and never writes to the branch it follows: it fetches and hard resets, so a change made anywhere else can't quietly become the truth.

Ours is at [rcmd_config](https://github.com/redcraft-org/rcmd_config) if you want to see what a real one looks like.

:warning: The checkout is a cache, not a workspace. rcmd hard resets it, so local edits in there WILL be thrown away. Edit the repository and let the change come back.

The database holds what happened, not what should happen: which versions were fetched, what a build resolved them to, who did what, and which tokens exist.

### Pins and versions

A template pins an artifact to something like `5.5.*`. `*` is the only wildcard and it matches at least one character, so a dot is a dot.

"Newest" means the most recently fetched version that matches, not the highest number. Version strings across sixty plugins don't share a scheme, and comparing `1.21.4-SNAPSHOT-b31` against `26.2` is not something a sort order can do honestly.

Versions are read out of the jar, from `plugin.yml`, `bungee.yml`, `paper-plugin.yml`, `velocity-plugin.json`, `fabric.mod.json`, `version.json` or `install.properties` depending on what the thing is. Never from the file name, because a plugin with a dash in its name is not a version (ask Multiverse-Core, which spent months at version "Core").

A couple of things carry no version anywhere inside the file. Those get `from_source` in their `post_processors`, which takes whatever the source called the release. It's not as good as reading it out of the jar, but it beats guessing.

### The pipeline

One job, every night: fetch everything the catalog lists, work out which templates pin something that changed, and rebuild only those.

Nothing is deployed by that. A built tar sits there until a server restarts, which is the same contract as before.

An artifact that comes back with a version outside its pin doesn't get shipped either. rcmd pushes a branch and opens a pin bump for review, because that's a decision somebody should make. Running the same bump twice doesn't force push over a branch you may already be reading.

A few plugins are paid and sit behind a bot check. Those are `source: manual` in the catalog, and you upload the jar yourself:

`curl -T CMI9.8.1.jar -H "Authorization: Bearer <your_token>" http://127.0.0.1:8484/api/v1/artifacts/CMI/versions`

It gets deduplicated, pinned, built and deployed like anything else.

### Applying a template

A template is a tar. Applying it removes and rewrites every path the tar names, and leaves everything else alone.

Basically, if `plugins/` is in the tar then your whole `plugins/` directory is deleted and rebuilt from the template. Your worlds, logs and caches aren't in the tar, so they survive.

:warning: :warning: :warning: If you store data inside a directory that the template also ships, applying a template will DELETE it! Plugin data directories under `plugins/` are the usual victim.

Template tars are also never trusted with paths. An entry trying to write outside the server directory is refused instead of applied, because "the bucket is ours" isn't a security model.

### Crashes and restarts

rcmd checks every second whether a server it believes is running still has a session. If it doesn't, that's a crash, and rcmd applies a fresh template before starting it again. This is on purpose: a crash is exactly the moment you want the server put back the way the template says it should be.

An explicit restart doesn't do that, unless you ask for it. Restarting from the web interface gives you both buttons.

By default, a server that crashes more than 3 times in 120 seconds is left down and marked as a bootloop, because reviving it a fourth time won't fix it either. Starting it yourself clears that, a crash revive doesn't.

### Who may do what

Every change, from every surface, goes through the same check and lands in the same audit log. The web interface, the API and MCP can't drift apart on what's allowed, because there's only one place that decides.

There are three roles. A `viewer` reads, an `operator` runs servers and builds, an `admin` also hands out tokens.

On top of the role, a few rules apply to everybody:

- A stop or a restart is refused while players are connected. You can pass `force` to do it anyway
- If `player_count_url` is set and doesn't answer, that counts as busy, not as empty. Guessing wrong drops a full server, and the one time it matters is the one time the API is down
- A server in `frozen_servers` is never restarted, and `force` doesn't change that
- Pushes to a public repository only happen inside `push_window`, unless you're an admin

The web interface asks before it draws the buttons, so a restart that would drop two people is greyed out with the reason on it, and one you're allowed to override turns red and asks first.

Tokens are `rcmt_` followed by a secret, and only the sha256 is stored. Losing one means making a new one, which is the point.

### Console output

Anything a player can type ends up in the console, so the API marks console responses as untrusted and the web interface shows a banner. Over MCP it comes back inside a fence that's measured against the content, so somebody typing a code block into chat can't close it and keep writing outside.

Keep that in mind if you ever pipe this into something that reads instructions, like an LLM (yes, that's the point).

## API

The web interface is just a client of this, it has no privileged path of its own. Everything needs an `Authorization: Bearer <your_token>` header.

- `GET /api/v1/servers` lists every server with its state
- `GET /api/v1/servers/<server_name>` for a single one
- `GET /api/v1/servers/<server_name>/preflight` says what the rules would allow, without doing anything
- `POST /api/v1/servers/<server_name>/start`, `/stop`, `/restart`, `/backup`
- `POST /api/v1/servers/<server_name>/command` with a JSON body like `{"command": "say hello"}`
- `GET /api/v1/servers/<server_name>/console?lines=100`
- `GET /api/v1/artifacts` and `GET /api/v1/artifacts/<artifact_name>/versions`
- `POST /api/v1/artifacts/<artifact_name>/versions` to upload a jar by hand
- `GET /api/v1/templates` and `POST /api/v1/templates/<template_name>/build`
- `POST /api/v1/pipeline/run` and `POST /api/v1/config/reload`
- `GET`/`POST /api/v1/tokens`, `DELETE /api/v1/tokens/<token_id>`
- `GET /api/v1/audit` for what everybody has been doing
- `GET /api/v1/whoami`, `GET /healthz`, `GET /version`

`restart` takes `{"update_template": true}` or `false` (true by default), `{"warn_minutes": 15}`, and `{"force": true}`.

A refusal you can override comes back as a 409 with `"overridable": true`. One you can't is a 403.

## MCP

rcmd speaks MCP on `POST /mcp`, with the same token and the same rules:

`claude mcp add --transport http rcmd http://127.0.0.1:8484/mcp --header "Authorization: Bearer <your_token>"`

The tools are thin wrappers around the API, so an assistant gets refused for exactly the reasons you would, and the refusal reads the same. There's a `check_first` tool that returns the verdicts without doing anything, which is worth asking before proposing a restart.

## Checking your config

`rcmd validate <config_directory>` loads a config directory and complains about it without running anything: a pin naming an artifact the catalog doesn't have, a template inheriting from something that isn't there, a loop in the chain, a placeholder that appears in no config file.

That last one is worth having. A secret that substitutes to nothing looks exactly like an empty password, and you find out at boot.

Please notice that the config repository can run this in its own CI, so a broken pin gets caught by a robot on a pull request rather than at 3am by you.

## What a template apply will and will not touch

Some files come back different after a server has been running, and that's normal:

- Paper, Purpur and Spigot rewrite `bukkit.yml`, `spigot.yml`, `purpur.yml`, `pufferfish.yml`, `server.properties` and `config/paper-global.yml` at boot, filling in defaults and a version marker
- Plugins do the same to their own config, LuckPerms and FastAsyncWorldEdit among them
- CMI reads `portals.yml`, `kits.yml` and `dataBaseInfo.yml` at boot and then DELETES them, which makes them look unused right up until you remove one from the template and the portals stop working (yes, that happened)

:warning: A file that isn't on the server doesn't mean the template doesn't need it. Check what the plugin does with it before taking it out.

## Contributing

You are free to suggest changes by opening an issue ticket.

You can also open PRs, remember to bump the version in `Cargo.toml` before opening a pull request.
