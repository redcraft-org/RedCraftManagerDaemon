# rcmd — RedCraftManagerDaemon

One service that builds, ships and runs a Minecraft server fleet: plugin
catalog, layered templates, deployment, process supervision, and the API,
web and MCP surfaces over all of it.

It replaces three tools that used to coordinate by cron offset and a
fire-and-forget Redis channel:

| Was | Did | Now |
|---|---|---|
| `redcraft_plugins_updater` | downloaded plugin jars into a bucket | artifact catalog + manifest |
| `redcraft_server_templates` | built server tars from layered configs | template builder |
| `redcraft_server_management` | ran the servers, applied the tars | server supervision |

## Design principle

Easy for a first server, configurable for a fleet. Every external
dependency is optional with a working local default; a large deployment
turns the power options on.

| Concern | Default | Power option |
|---|---|---|
| Database | SQLite file | MySQL |
| Artifact + template storage | local directories | S3 |
| Config source | local directory | a public git repo, PR-reviewed |
| Restart warning | broadcast, wait, stop | a plugin command such as `reboot 15 <reason>` |
| Auth | CLI-issued API tokens | Microsoft OAuth linked to Minecraft accounts |

## Status

Early. Server supervision and its REST surface work; the catalog,
template builder, scheduler, web UI and MCP surface are being built on
top. See `docs/` for the roadmap.

## Quick start

```sh
cargo build --release
./target/release/rcmd init            # writes config.toml
./target/release/rcmd --config config.toml serve
curl localhost:8484/api/v1/servers
```

Every directory under `servers_directory` is a server. Each needs an
`rcsm_config.json` with `start_command`, `stop_command` and
`directories_to_backup`; rcmd writes a default one if it is missing.

## Behaviour worth knowing

**Servers run in detached tmux sessions.** rcmd can restart without
touching a single running server, and it adopts whatever sessions already
exist at boot. You can still `tmux attach` to any of them.

**Applying a template is a wipe-and-replace, per entry.** Anything named
in the template tar is removed and rewritten — so `plugins/` is rebuilt
wholesale on every apply. Anything *not* in the tar survives: worlds,
logs, caches. A hand edit to a templated file will be lost on the next
apply, by design.

**A crash pulls a fresh template; a restart does not.** Reviving a
crashed server is the moment to re-assert the template. An explicit
restart leaves the disk alone unless you ask for `update_template`.

**Bootloops stop.** More than `crash_max_tries` revives inside
`crash_window_secs` and the server is left down for a human. Starting it
yourself clears the history.

**Console output is fenced.** Anything a player can influence comes back
with `"untrusted": true` and a label, so no client forgets that chat is
attacker-controlled text.

## API

```
GET  /healthz  /version
GET  /api/v1/servers            list with live session state
GET  /api/v1/servers/{name}
POST /api/v1/servers/{name}/start | stop | restart | backup
POST /api/v1/servers/{name}/command   {"command": "say hi"}
GET  /api/v1/servers/{name}/console?lines=100
```

`restart` takes `{"update_template": true|false}`, defaulting to true.

## License

MIT
