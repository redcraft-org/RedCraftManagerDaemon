// rcmd web UI.
//
// Plain ES modules against the same REST API every other surface uses, so
// the browser has no privileged path: what it can do, a token can do.
// No build step on purpose while the surface is this small; a bundler can
// come later without moving the API underneath it.

const REFRESH_MS = 3000;
const CONSOLE_MS = 2000;

let openConsole = null;
let consoleTimer = null;

async function api(path, options = {}) {
  const response = await fetch(`/api/v1${path}`, {
    headers: { "content-type": "application/json" },
    ...options,
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `${response.status}`);
  return body;
}

function toast(message, kind = "") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = message;
  document.getElementById("toasts").append(el);
  setTimeout(() => el.remove(), 6000);
}

/// The state badge, including the disagreement the healthcheck exists to
/// resolve: what rcmd believes versus whether a session is actually alive.
function stateBadge(server) {
  const label = server.state === "crashed"
    ? (server.bootloop ? "bootloop" : "crashed")
    : server.state;
  const cls = ["running", "stopped", "crashed"].includes(server.state)
    ? server.state
    : "transition";
  return `<span class="state ${cls}">${label}</span>`;
}

function detailLine(server) {
  const bits = [];
  const believesUp = server.state === "running" || server.state === "starting";
  if (believesUp !== server.session_alive) {
    bits.push(`<span class="disagree">session ${server.session_alive ? "alive" : "gone"}, not yet reconciled</span>`);
  }
  if (server.restart_tries > 0) {
    bits.push(`${server.restart_tries} revive${server.restart_tries === 1 ? "" : "s"} in this window`);
  }
  return bits.join(" · ") || "&nbsp;";
}

function card(server) {
  const running = server.state === "running" || server.state === "starting";
  return `
    <div class="card" data-server="${server.name}">
      <div class="card-top">
        <span class="name">${server.name}</span>
        ${stateBadge(server)}
      </div>
      <div class="detail">${detailLine(server)}</div>
      <div class="actions">
        <button data-act="start"   ${running ? "disabled" : ""}>start</button>
        <button data-act="stop"    ${running ? "" : "disabled"}>stop</button>
        <button data-act="restart" ${running ? "" : "disabled"}>restart</button>
        <button data-act="restart-template" ${running ? "" : "disabled"}>restart + template</button>
        <button data-act="console" class="ghost" ${running ? "" : "disabled"}>console</button>
        <button data-act="backup"  class="ghost">back up</button>
      </div>
    </div>`;
}

async function refreshServers() {
  try {
    const servers = await api("/servers");
    document.getElementById("servers").innerHTML = servers.map(card).join("");
    document.getElementById("servers-empty").hidden = servers.length > 0;
    document.getElementById("refreshed").textContent =
      `updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    document.getElementById("refreshed").textContent = `unreachable: ${e.message}`;
  }
}

async function act(name, action) {
  const paths = {
    start: ["/start", {}],
    stop: ["/stop", {}],
    restart: ["/restart", { update_template: false }],
    "restart-template": ["/restart", { update_template: true }],
    backup: ["/backup", {}],
  };
  const [suffix, body] = paths[action];
  try {
    const result = await api(`/servers/${encodeURIComponent(name)}${suffix}`, {
      method: "POST",
      body: JSON.stringify(body),
    });
    if (action === "backup") {
      const kb = Math.round(result.backup.bytes / 102.4) / 10;
      const missing = result.backup.missing.length
        ? ` (missing: ${result.backup.missing.join(", ")})`
        : "";
      toast(`${name}: backed up ${kb} kB${missing}`, "ok");
    } else {
      toast(`${name}: ${action}`, "ok");
    }
  } catch (e) {
    toast(`${name}: ${e.message}`, "err");
  }
  refreshServers();
}

async function refreshConsole() {
  if (!openConsole) return;
  try {
    const data = await api(`/servers/${encodeURIComponent(openConsole)}/console?lines=300`);
    const body = document.getElementById("console-body");
    const pinned = body.scrollTop + body.clientHeight >= body.scrollHeight - 24;
    body.textContent = data.content.trimEnd();
    // The fence the API sets is rendered, never assumed: if a future
    // endpoint stops marking output as player-influenced, the banner goes
    // away with it rather than lying.
    document.getElementById("console-warn").hidden = !data.untrusted;
    if (pinned) body.scrollTop = body.scrollHeight;
  } catch (e) {
    document.getElementById("console-body").textContent = `console unavailable: ${e.message}`;
  }
}

function showConsole(name) {
  openConsole = name;
  document.getElementById("console-name").textContent = name;
  document.getElementById("console-section").hidden = false;
  document.getElementById("console-body").textContent = "loading…";
  refreshConsole();
  clearInterval(consoleTimer);
  consoleTimer = setInterval(refreshConsole, CONSOLE_MS);
  document.getElementById("console-section").scrollIntoView({ behavior: "smooth" });
}

function hideConsole() {
  openConsole = null;
  clearInterval(consoleTimer);
  document.getElementById("console-section").hidden = true;
}

document.getElementById("servers").addEventListener("click", (event) => {
  const button = event.target.closest("button[data-act]");
  if (!button) return;
  const name = button.closest("[data-server]").dataset.server;
  const action = button.dataset.act;
  if (action === "console") return showConsole(name);
  act(name, action);
});

document.getElementById("console-close").addEventListener("click", hideConsole);

document.getElementById("command-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = document.getElementById("command-input");
  const command = input.value.trim();
  if (!command || !openConsole) return;
  try {
    await api(`/servers/${encodeURIComponent(openConsole)}/command`, {
      method: "POST",
      body: JSON.stringify({ command }),
    });
    input.value = "";
    setTimeout(refreshConsole, 400);
  } catch (e) {
    toast(`command failed: ${e.message}`, "err");
  }
});

fetch("/version")
  .then((r) => r.json())
  .then((v) => {
    document.getElementById("version").textContent = `v${v.version}`;
  })
  .catch(() => {});

refreshServers();
setInterval(refreshServers, REFRESH_MS);
