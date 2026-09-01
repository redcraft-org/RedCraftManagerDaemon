# One tool to replace rcpu + rcst + rcsm — Rust service on the MC box

## Context

Three tools currently run RedCraft's fleet: **rcpu** (Python, GH Actions, downloads plugin jars to S3), **rcst** (Python, GH Actions, builds server template tars from layered configs + pinned jars), **rcsm** (Go daemon on the box, runs servers in tmux, applies tars, revives crashes). They coordinate by *cron offset* (03:00 vs 04:00) and a fire-and-forget Redis channel; there is no API, no status read, no record of what's deployed where, and control happens via an ambient-authority MCP server on Lucas's laptop (SSH + open Redis + local file edits). This week's incidents — stale pin silently reinstalling old builds, template regenerated before the updater uploaded, config drift between template and live — are all consequences of that shape.

The goal: **`rcmd` — RedCraftManagerDaemon — one Rust service on the MC box** (the name is a triple play: it manages, it's your command surface, and it ends in `d` like a daemon binary should) owning the whole chain — catalog → artifacts → templates → deployment → process supervision — exposed through REST + web UI + native MCP (v1), Microsoft-OAuth-linked accounts (v1.5), in-game control via dialogs + DisplayKit (v2), and LLM-powered update analysis with auto-PRs (v3). Clean rewrite, one cutover, full rollback path.

**Decisions locked with the user**: Rust · all on the box · clean cutover · v1 = core parity + REST + per-user tokens + web UI + native MCP · all config in one new public GitHub repo, YAML, PR-able by the community · SPA embedded via rust-embed (a planner argued for HTMX server-rendered; user's explicit choice stands — REST-first design keeps the UI layer swappable, and the SPA must be vendored/lockfile-committed so it still builds after a dormant year) · Microsoft OAuth + per-user API tokens · name = **rcmd** (RedCraftManagerDaemon; repo `redcraft_manager_daemon`, binary/unit/journald `rcmd`, config repo suggestion `rcmd_config`).

## Design principle: newbie-easy, power-configurable

rcmd is a general tool, not RedCraft-only. Every infrastructure dependency is **optional with a local default**; RedCraft's deployment simply turns the power options on. `rcmd init` = interactive first-run wizard (detects servers dir, writes config.toml, creates first admin + token).

| Concern | Newbie default (zero external deps) | Power option (= RedCraft's deployment) |
|---|---|---|
| Database | SQLite file | MySQL (same instance as the MC servers, own `rcmd` DB + user) |
| Artifact/template storage | Local directories (rcpu/rcst's Basic destinations prove the shape) | S3 (Scaleway buckets, unchanged keys) |
| Config source | Plain local config dir, edited in place | Public GitHub repo, webhook/poll-followed read-only, PRs for changes |
| Restart warning | Configurable per server: `warn_command` template (`{minutes}`, `{reason}`) + wait; default = `say`-broadcast then stop | `reboot {minutes} {reason}` → RedCraftReboot bossbar/kick/shutdown flow |
| Auth | Token-paste login, CLI-bootstrapped admin | Microsoft OAuth + MC-linked accounts |
| Notifications | none | Discord webhook |

Storage layer written backend-agnostic (thin trait over sqlx, no backend-specific SQL, migrations per backend). The warn-then-restart flow must NOT assume RedCraftReboot's shutdown side effect: generic path is warn → wait → stop → (template) → start; RedCraft's config relies on the plugin's self-shutdown + crash-revive contract as today.

## Semantics that MUST be preserved exactly (source files)

| Behavior | Source |
|---|---|
| Template layering: parent-first recursion into ONE dir, whole-file copytree replacement (deliberate, user-built), resources as `{type}/{Name}.jar` version-stripped, env substitution line-regex over ext whitelist `yml yaml conf toml json properties ini csv secret`, `.strip()` on values | `redcraft_server_templates/templates/manager.py:46-77,224-241` |
| Wildcard pins: `re.escape` then `*`→`.+`, anchored; "latest" = most-recent-upload among matches (now stable: manifest rows inserted only on sha change) | `manager.py:167-186,256-260` |
| Pin widening on update | `redcraft_server_templates/utils/updater.py::generate_new_wildcard_version` — port verbatim |
| tmux verbs: `send-keys -l` + separate Enter, session prefix `rcsm_`, wait-poll, kill-on-timeout | `redcraft_server_management/rcsm/tmux.go` |
| Tar apply: per-entry RemoveAll-then-write (in-tar = wiped, not-in-tar = survives: worlds/logs), root guard; ADD `..`/absolute-path rejection | `rcsm/provisioning.go:75-125` |
| Crash-revive pulls template; plain restart NEVER does (MCP's `update_template=True` = stop + let healthcheck revive); bootloop = 3 tries/120s | `rcsm/healthcheck.go:59-62`, `servers.go:50-56` |
| Warn-then-restart: console `reboot 15 <reason>` (RedCraftReboot does bossbar+kick+shutdown → revive pulls template), wait 900s, sweep restart | `redcraft_kubernetes/rcsm-cron-tasks/restart-cron.yaml` |
| Per-server config stays `{server_dir}/rcsm_config.json` `{start_command, stop_command, directories_to_backup}` — zero migration | `rcsm/servers.go:13-23`, shipped inside templates |
| Per-item download results | `redcraft_plugins_updater/.claude/worktrees/nifty-roentgen-6dfeea/download/manager.py` (DownloadResult + gather(return_exceptions) + tests) |
| Jar version extraction (bungee/plugin/paper-plugin.yml, velocity-plugin.json) | `redcraft_plugins_updater/download/post_processors/plugin_post_processor.py` |
| All 16 MCP guardrails (policy table below) | `redcraft_mcp/server.py` (line refs in table) |

## Architecture

Single binary crate. **SQLite by default, MySQL as a first-class option** (RedCraft runs MySQL: the 10.0.2.42 instance RCC talks to, new `rcmd` DB + dedicated user); nightly dump → storage backend. **What the DB is for — runtime state and history ONLY, never config**: artifact manifest (version/sha/uploaded_at index over the S3 bucket), template_builds + items (what each build resolved each pin to), deployments (what is actually on which server), jobs/job_items, users/api_tokens/sessions/link_codes, audit_log, and later update analyses. S3 buckets unchanged in v1 (flat `{name}-{version}.jar` keys = rollback guarantee). **All configuration lives in ONE new public GitHub repo, in YAML** (user decision): catalog + template definitions + policy data (frozen servers, drift ignore rules) consolidated from the two current repos so the community can read and PR it. Layout: `catalog.yml` (entries gain the required `kind: engine|plugin|mod`), `templates/<name>/template.yml` + `config/` trees, `policy.yml`. **Git is the single source of truth for config; the service is a read-only follower**: local clone hard-reset to origin/master (never local commits on master, never overwrites external changes), synced by GitHub webhook where reachable (a tiny relay on the existing k8s website ingress can forward to the box over the VPN if the box isn't directly reachable) with a 60s poll fallback either way; `config_git_sha` recorded per build. The service writes config exclusively through `update/*` branches pushed for PRs (pin bumps), reviewed like anyone else's. Rules: **placeholders only, never secrets** (substitution values injected at build from the box env — already how it works; rcpu's committed `.env` must NOT migrate); branch protection + CODEOWNERS; a PR validation workflow in that repo (YAML schema check + dry-run template build via `rcmd validate --remote` or a container) so community PRs get CI feedback; service builds only from master. YAML via serde with typed schemas (avoids the Norway-problem class of YAML surprises); one-time mechanical converter from the existing `redcraft_plugins.json` + `rcst_template_*.json` during M2/M3, old repos archived at cutover. Keep tmux (detached sessions survive daemon restarts → zero-downtime cutover + operator attach). Per-server tokio actor replaces the one-global-lock; `Result` returns replace void-log-and-os::Exit.

```
src/: main config domain store/ s3 gitconfig(octocrab PRs) 
  artifacts/{sources/(github modrinth jenkins papermc direct fabricmc zrips spiget spigotmc) post/}
  templates/{builder envsubst} servers/{tmux actor health apply backup}
  pipeline scheduler jobs policy api/ notify(Discord)
```
Crates: tokio axum tower sqlx aws-sdk-s3 reqwest redis octocrab serde{,_json,_yaml} zip tar flate2 regex croner totp-rs sha2 scraper tracing rust-embed rmcp(pinned).

**Domain**: `artifacts(name PK, kind engine|plugin|mod ← NEW required field, source, file_filter, post_processors, enabled)` · `artifact_versions(name, version, sha256, s3_key, uploaded_at)` — kills first-dash parsing (Multiverse bug) · `template_builds` + `template_build_items(pin, resolved_version)` = provenance · `servers(name, template_name, managed)` + runtime state machine · `deployments(server, build, trigger, at)` = the missing "what's actually where" · `jobs` + `job_items` (DownloadResult generalized) · `users/api_tokens/sessions/link_codes/audit_log`.

**Pipeline as one job** (replaces cron offsets): config git-ff → update check (ETag/HEAD polling, NO blind downloads) → download new only (retry/backoff, item-failure ≠ run-failure) → manifest insert → impact analysis (pin reverse-index) → rebuild affected templates → pin-bump PRs → mark pending-apply (apply only at restart window or on demand — preserves today's contract). Scheduler absorbs: 03:00 pipeline, 05:45 warn+900s+06:00 restart-all-with-template (guarded: waits for pipeline; warn via per-server `warn_command` template), 6h backups, nightly DB snapshot → storage.

**Template builder fixes** (behind golden-master gate): tar created OUTSIDE build dir (self-tar bug), engine resolved once (double-download), missing env var → loud warn + build-result record (today: silent empty string), `config_env_matches` accumulate down chain (currently parent declarations are dead; values identical in practice), S3 listing paginated.

**Redis**: instance stays for RCC's cache; rcmd talks to it only as a client for the cache-clear endpoint (P15). **No rcsm-channel compat listener** (user: useless) — the k8s CronJobs' schedules move into rcmd's scheduler at cutover and the MCP surface swaps to native at the same time; the old channel just goes dark.

**SpigotMC risk shrunk 7→3**: CMILib → github source (release jars verified present); PlotSquared/FarmLimiter/SelectionVisualizer → new `spiget` source (plain JSON API; verify spiget serves PlotSquared's file, else manual bucket); CMI/HeadDatabase/LiteBans (premium) → FlareSolverr-only flow (drop cloudscraper; FlareSolverr owns challenge+cookies+login POST, totp-rs MFA, reqwest replays cookies, keep 5s throttle) best-effort + alert + `POST /artifacts/{name}/versions` manual-upload fallback.

## REST (all `/api/v1`, long ops → `202 {job_id}`)

`GET/POST servers/{name}/{start,stop,restart{update_template,warn_seconds,force},command,console,console/stream(SSE),backup,deployments}` · `artifacts` + `/refresh` + manual version upload · `catalog/entries` + `/pin` (→ git PRs; absorbs MCP add_plugin_to_catalog/pin_plugin) · `templates/{name}/build`, `template-builds/{id}` · `deployments` (matrix) · `jobs`, `pipeline/run`, `events`(audit), `stream`(SSE), `healthz`.

## Auth & policy (shared by ALL surfaces)

- **v1**: per-user API tokens `rcmt_<prefix><secret>` (prefix + sha256 at rest, shown once, revoke/last-used in UI), CLI bootstrap for first admin, token-paste web login → session. Roles viewer/operator/admin.
- **v1.5 OAuth**: consumers-tenant Azure app, XboxLive.signin, PKCE+secret; chain MSA→XBL→XSTS(rp api.minecraftservices.com)→login_with_xbox→/minecraft/profile→UUID. **GATE: Mojang must whitelist the client ID (403 otherwise) — submit the approval form on day 1 of v1.** Tiers: (2) in-game link code `/panel link <code>` confirmed by RCC over localhost — ships FIRST, works regardless of Mojang; (3) Bedrock XUID via XSTS rp xboxlive.com (no whitelist) → `UUID(0, xuid)`.
- **In-game actor**: RCC holds `surface:ingame` service token + `X-RedCraft-Actor: <uuid>`; service authorizes by role-on-UUID; LuckPerms admin only gates command *visibility*.
- **Policy module** — every mutation from every surface through `policy::check(ActorContext{user, role, surface: Web|Mcp|InGame|System}, Action)`; System = scheduled jobs, still audited; denials audited. Port table (all from `redcraft_mcp/server.py`): P1 restart-refusal when non-admins online, FAIL CLOSED (:274-287) · P2 warn+10s randomized French broadcast (:499-518) · P3 proxy=everyone · P4 push-window 17:00 Quebec weekdays for public-repo pushes, admin override (:289-294) · P5 frozen flag per server (crea_engineer_test) (:68) · P6 untrusted-fencing applied at source — REST envelope flag, MCP literal fence, web banner (:230-239) · P7 unknown server → 404 · P8 kind minecraft|process, no template-restart/holograms on process (bluemap) (:130) · P9 git: master-only, `add -u` only, no AI attribution · P10 ordering → job DAG · P11 `minecraft:` qualification vs CMI shadowing (:775-795) · P12 structured command transport + delivery check · P13 literal log search · P14 holograms template-first, transactional · P15 cache deletes only under `RCC;` · P16 per-server stop command.

## Web UI (v1, SPA embedded)

Pages: Dashboard (server cards, push-window badge, running jobs) · Servers (+ policy preflight shown BEFORE click: "would drop 2 non-admins") · Console (SSE tail + untrusted banner + literal filter + command box) · Artifacts · Templates (pins, layer chain, diff vs deployed) · Deployments (matrix + pipeline runs) · Jobs · Users&Tokens · Audit. Console streaming = **SSE not WebSocket** (one-directional; input is a POST; Last-Event-ID reconnect; same endpoint later feeds the in-game terminal). TLS via Caddy in front (`manage.redcraft.org`, needed for OAuth redirect anyway).

## Native MCP (v1)

rmcp (official SDK, stdio + streamable HTTP; pin version) mounted at `/mcp` on the same router; tools = thin adapters → same ActorContext → same service layer → fence rendering. ~1:1 with today's tools (mapping table in planner output). Lucas's migration = config-only: `{"type":"http","url":"http://rcmc.wg:8484/mcp","headers":{"Authorization":"Bearer rcmt_…"}}`; parity week alongside the Python server, then retire it (ambient authority → per-user tokens + audit = the security win of the project).

## In-game (v2)

RCC `management` package (NOT a new plugin — DisplayKit-Velocity is a host-embedded library and RCC already hosts it; own `redcraftadmin:` dialog namespace; reuse NativeDialogSelector's version gate + input-key kick guard). **Dialog tier first** (1.21.6+ incl. Bedrock via Geyser): server list → status/actions → confirmations showing policy verdicts verbatim; typed inputs (pins, filters, link codes) via TextInputControl. **DisplayKit tier** (1.21.9+ Java): SurfaceWindow + TabbedView, PagedDataListView status, TerminalWidget console fed by the SSE stream — command input via dialog button (Velocity TextInput is stubbed, chat-capture is Fabric-only — verified). panel-lib = client-side Fabric ImGui overlay; optional someday admin console, just another REST client; out of scope.

## LLM update intelligence (v3, order U1→U6)

- **U1 event loop**: per-source pollers (GitHub ETag, Modrinth versions[0], Jenkins, Paper fill, HEAD, FlareSolverr+5s bucket; hourly→12h cadences) → ingest → manifest → matcher: within-pin = minor → stage+diff+apply at window; outside = major → analysis ∥ proposal (PR NEVER waits on LLM).
- **U2 tar-diff**: entry manifests `[{path, sha256}]` per build & per applied server; hash diff + unified text diffs; feeds dry-run UI, PR bodies, LLM evidence. **Secrets re-masked via config_env_matches map before any display/PR/prompt.**
- **U3 config drift** (this week's incidents): hourly + pre-apply scan vs applied manifest; per-file modes `expect_rewritten` (paper-global.yml) / `expect_consumed` (CMI portals.yml) / `ignore`; pre-apply warns "apply will clobber this live edit".
- **U4 auto-PRs**: `change_proposal` rows; one branch per update `update/<artifact>-<version>`, force-push + supersede (fixes shared-branch + PR-body-mutation bugs in `utils/git_manager.py`); body = risk badge + config table + changelog + collapsed tar-diff; auto-merge OFF by default.
- **U5 mechanical analysis (zero LLM, ~60% of value)**: jar-resource diff (embedded default configs + plugin.yml commands/perms/depend — catches the CMI-portals class mechanically, obfuscation-proof) + classfile structural diff (cafebabe/noak; string-constant pool = config-key signal; javap fallback).
- **U6 model analysis**: relevance ranking (+3 keys∩server's actual configs, +3 event/API, +2 serialization/SQL, top-12) → Vineflower on changed classes only (`-dgs=1 -nls=1 -hdc=0 -jvn=0 -thr=1`, 512M, 120s kill, pinned) → one Messages call: claude-sonnet-5 primary, opus escalation on low-confidence/high-risk; cached system prompt; changelog first (cheapest signal); structured output `{risk, confidence, reasons[], config_changes[], recommended_actions[], needs_escalation}`. ~$0.08/analysis, ~$10/heavy month. Advisory ALWAYS: risk:unknown ≡ manual review, never blocks; obfuscated → skip decompile, cap confidence; engines → changelog-only; injection: everything fenced as data, no tools in the call.

## Milestones

**v1**: M0 scaffold → M1 servers core (tmux/actors/health/apply/backup + read-only REST; shadow-deployable) → M2 artifacts (manifest, sources, post-processors, `import-legacy` backfill, doctor) → M3 template engine + **golden-master gate: Python rcst vs Rust builder over all 10 templates, tar trees must match modulo known-bug fixes — non-negotiable before cutover** → M4 pipeline/scheduler/jobs/deployments/Redis-compat/Discord/pin-PRs → M5 auth seam + tokens + audit → M6 web UI → M7 MCP surface + Lucas parity week → M8 SpigotMC risk work → **M9 cutover**.
**v1.5** OAuth + linking (link codes first). **v2** in-game. **v3** U1–U6.

## Cutover (M9) — invariants: no S3 layout/tmux-name/rcsm_config changes; old binaries stay on disk

1. Scratch-VM test + golden master. 2. **Shadow**: install alongside rcsm (`scheduler=off, read_only=true`), `import-legacy`, watch status parity for days. 3. **Freeze** after a good 06:00: disable both GH Action schedules, suspend both k8s CronJobs. 4. **Swap**: `systemctl disable --now rcsm` (tmux sessions live on, players unaffected) → rcmd full mode, adopts sessions; cut the MCP config over to rcmd's `/mcp` in the same window (the Redis channel goes dark — no bridge); verify console read, restart, backup, build. 5. Supervise first 03:00 + 06:00 cycle. 6. Delete k8s crons, archive workflows, update packer (install_rcmd.sh + unit). 7. **Rollback** = re-enable rcsm + unsuspend crons + re-enable workflows; works because all keys/formats unchanged; keep rcsm ≥ a month.

## Verification

- Unit: pin regex vs Python outputs; generate_new_wildcard_version case table from `tests/test_updater.py`; tar traversal (incl. `./`-prefixed entries from `arcname='.'`); envsubst whitespace cases from rcst tests; jar-meta extraction against `tests/fixtures/fake_plugin.jar`.
- **M3 golden-master harness** (the big one): both builders, same env/bucket, diff extracted trees per template.
- Doctor pre-cutover: Rust vs Python version resolution over the live bucket must agree per pin.
- Integration: fake crashing server script → bootloop math; healthcheck revive pulls template; policy P1 fail-closed with players API down.
- Shadow-mode soak = live read-parity test. MCP parity week = surface test.
- Post-cutover watch: first pipeline (03:00) and warn+restart (05:45/06:00) supervised end-to-end.

## Risks (top 5)

1. Template build fidelity (silent config corruption) → golden-master gate. 2. Mojang client-ID approval latency → link codes are a full substitute; submit day 1. 3. Cutover session adoption → shadow soak before write authority. 4. SpigotMC/Cloudflare rot → capped at 3 premium plugins + manual upload. 5. Version-resolution drift at import boundary → doctor diff before cutover.
