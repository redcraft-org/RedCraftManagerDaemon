// The web interface. Plain JavaScript on purpose: this file is embedded in
// the binary, so a build step here would mean the daemon cannot be built
// from a checkout without a working Node install, and that is not a trade
// worth making for a page with six tabs on it.

const $ = (id) => document.getElementById(id);
const TOKEN_KEY = "rcmd.token";

let token = null;
let me = null;
let openConsole = null;
let timer = null;

// Anything a player can influence is rendered as text, never as markup.
function text(value) {
  return document.createTextNode(value === null || value === undefined ? "" : String(value));
}

function el(tag, className, contents) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (contents !== undefined) node.appendChild(text(contents));
  return node;
}

function toast(message, kind) {
  const node = el("div", "toast" + (kind ? " " + kind : ""), message);
  $("toasts").appendChild(node);
  setTimeout(() => node.remove(), 6000);
}

async function api(path, options) {
  const settings = Object.assign({ headers: {} }, options || {});
  settings.headers = Object.assign(
    { Authorization: "Bearer " + token },
    settings.headers
  );
  if (settings.body !== undefined) {
    settings.headers["Content-Type"] = "application/json";
    settings.body = JSON.stringify(settings.body);
  }
  const response = await fetch(path, settings);
  if (response.status === 401) {
    signOut("That token is not valid any more.");
    throw new Error("unauthenticated");
  }
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new Error(body.error || response.statusText);
    error.overridable = body.overridable === true;
    error.status = response.status;
    throw error;
  }
  return body;
}

/* Sign in */

function signOut(why) {
  token = null;
  me = null;
  clearInterval(timer);
  try {
    localStorage.removeItem(TOKEN_KEY);
  } catch (e) {
    // A browser with storage switched off still signs in, it just forgets.
  }
  $("app").hidden = true;
  $("login").hidden = false;
  if (why) {
    $("login-error").textContent = why;
    $("login-error").hidden = false;
  }
}

async function signIn(candidate) {
  token = candidate;
  me = await api("/api/v1/whoami");
  try {
    localStorage.setItem(TOKEN_KEY, candidate);
  } catch (e) {
    // Not being able to remember the token is not a reason to refuse it.
  }
  $("login").hidden = true;
  $("app").hidden = false;
  $("whoami").textContent = me.name + " (" + me.role + ")";
  for (const node of document.querySelectorAll(".admin-only")) {
    node.hidden = me.role !== "admin";
  }
  const version = await api("/version");
  $("version").textContent = "v" + version.version;
  show("servers");
  timer = setInterval(refresh, 5000);
}

$("login-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  $("login-error").hidden = true;
  const candidate = $("login-token").value.trim();
  if (!candidate) return;
  try {
    await signIn(candidate);
    $("login-token").value = "";
  } catch (e) {
    token = null;
    $("login-error").textContent = "That token was not accepted.";
    $("login-error").hidden = false;
  }
});

$("sign-out").addEventListener("click", () => signOut(null));

/* Pages */

const PAGES = ["servers", "artifacts", "templates", "pipeline", "tokens", "audit"];
let current = "servers";

function show(page) {
  current = page;
  for (const name of PAGES) {
    $("page-" + name).hidden = name !== page;
  }
  for (const tab of document.querySelectorAll(".tab")) {
    tab.setAttribute("aria-current", tab.dataset.page === page ? "true" : "false");
  }
  refresh();
}

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => show(tab.dataset.page));
}

async function refresh() {
  if (!token) return;
  try {
    if (current === "servers") await loadServers();
    if (current === "artifacts") await loadArtifacts();
    if (current === "templates") await loadTemplates();
    if (current === "tokens") await loadTokens();
    if (current === "audit") await loadAudit();
    if (openConsole) await loadConsole();
    $("refreshed").textContent = new Date().toLocaleTimeString();
  } catch (e) {
    if (e.message !== "unauthenticated") toast(e.message, "bad");
  }
}

/* Servers */

async function loadServers() {
  const servers = await api("/api/v1/servers");
  const grid = $("servers");
  grid.replaceChildren();
  $("servers-empty").hidden = servers.length > 0;

  for (const server of servers) {
    const card = el("div", "card");
    const head = el("div", "card-head");
    head.appendChild(el("h3", null, server.name));
    const state = typeof server.state === "string" ? server.state : "crashed";
    head.appendChild(el("span", "state " + state, state));
    card.appendChild(head);

    const detail = el("div", "sub");
    detail.appendChild(text(server.session_alive ? "session alive" : "no session"));
    if (server.restart_tries > 0) {
      detail.appendChild(text(" - " + server.restart_tries + " crash(es) recently"));
    }
    card.appendChild(detail);

    const actions = el("div", "actions");
    actions.appendChild(action(server.name, "start", "start"));
    actions.appendChild(action(server.name, "stop", "stop"));
    actions.appendChild(action(server.name, "restart", "restart"));
    const backup = action(server.name, "backup", "backup");
    backup.className = "ghost";
    actions.appendChild(backup);
    const consoleButton = el("button", "ghost", "console");
    consoleButton.addEventListener("click", () => openConsoleFor(server.name));
    actions.appendChild(consoleButton);
    card.appendChild(actions);

    grid.appendChild(card);

    // What the policy layer would say, before anybody clicks anything. The
    // point is reading "would drop 2 players" instead of finding out.
    api("/api/v1/servers/" + encodeURIComponent(server.name) + "/preflight")
      .then((preflight) => {
        for (const [name, verdict] of Object.entries(preflight.actions)) {
          const button = card.querySelector('[data-action="' + name + '"]');
          if (!button) continue;
          button.disabled = !verdict.allowed && !verdict.overridable;
          button.title = verdict.reason;
          if (!verdict.allowed && verdict.overridable) {
            button.classList.add("danger");
          }
        }
      })
      .catch(() => {});
  }
}

function action(server, name, label) {
  const button = el("button", null, label);
  button.dataset.action = name;
  button.addEventListener("click", () => run(server, name, button));
  return button;
}

async function run(server, name, button) {
  const path = "/api/v1/servers/" + encodeURIComponent(server) + "/" + name;
  button.disabled = true;
  try {
    await api(path, { method: "POST", body: {} });
    toast(name + " on " + server + " done", "ok");
  } catch (e) {
    if (e.overridable && confirm(e.message + "\n\nDo it anyway?")) {
      try {
        await api(path, { method: "POST", body: { force: true } });
        toast(name + " on " + server + " done", "ok");
      } catch (again) {
        toast(again.message, "bad");
      }
    } else if (!e.overridable) {
      toast(e.message, "bad");
    }
  } finally {
    button.disabled = false;
    refresh();
  }
}

/* Console */

function openConsoleFor(name) {
  openConsole = name;
  $("console-name").textContent = name;
  $("console-section").hidden = false;
  $("console-body").textContent = "loading...";
  loadConsole();
}

$("console-close").addEventListener("click", () => {
  openConsole = null;
  $("console-section").hidden = true;
});

$("console-filter").addEventListener("input", () => loadConsole());

async function loadConsole() {
  if (!openConsole) return;
  const body = await api(
    "/api/v1/servers/" + encodeURIComponent(openConsole) + "/console?lines=400"
  );
  // The filter is a plain substring, not a pattern. Somebody searching a
  // console for "[1.21.4]" should find that line, not a regex error.
  const needle = $("console-filter").value;
  const lines = String(body.content).split("\n");
  const kept = needle ? lines.filter((line) => line.includes(needle)) : lines;
  const pane = $("console-body");
  const atBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
  pane.textContent = kept.join("\n");
  if (atBottom) pane.scrollTop = pane.scrollHeight;
}

$("command-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = $("command-input");
  const command = input.value.trim();
  if (!command || !openConsole) return;
  try {
    await api("/api/v1/servers/" + encodeURIComponent(openConsole) + "/command", {
      method: "POST",
      body: { command },
    });
    input.value = "";
    setTimeout(loadConsole, 400);
  } catch (e) {
    toast(e.message, "bad");
  }
});

/* Artifacts */

async function loadArtifacts() {
  const body = await api("/api/v1/artifacts");
  const needle = $("artifact-filter").value.toLowerCase();
  const artifacts = body.artifacts.filter((a) => a.name.toLowerCase().includes(needle));
  const rows = $("artifacts-table").querySelector("tbody");
  rows.replaceChildren();
  $("artifacts-empty").hidden = body.artifacts.length > 0;

  for (const artifact of artifacts) {
    const row = document.createElement("tr");
    const name = document.createElement("td");
    name.appendChild(el("span", "mono", artifact.name));
    if (!artifact.enabled) {
      name.appendChild(text(" "));
      name.appendChild(el("span", "pill", "disabled"));
    }
    row.appendChild(name);
    row.appendChild(cell(artifact.kind, "dim"));
    row.appendChild(cell(artifact.source, "dim"));

    const versions = document.createElement("td");
    versions.className = "right dim";
    versions.appendChild(text("..."));
    row.appendChild(versions);
    rows.appendChild(row);

    api("/api/v1/artifacts/" + encodeURIComponent(artifact.name) + "/versions")
      .then((found) => {
        versions.replaceChildren();
        const newest = found.versions[found.versions.length - 1];
        if (!newest) {
          versions.appendChild(el("span", "pill", "never fetched"));
          return;
        }
        versions.appendChild(el("span", "mono", newest.version));
        if (!newest.present) {
          versions.appendChild(text(" "));
          // A manifest row whose jar has gone missing breaks the next build,
          // and finding that out at build time is too late.
          versions.appendChild(el("span", "pill missing", "file missing"));
        }
      })
      .catch(() => {});
  }
}

$("artifact-filter").addEventListener("input", () => loadArtifacts());

function cell(value, className) {
  const node = document.createElement("td");
  if (className) node.className = className;
  node.appendChild(text(value));
  return node;
}

/* Templates */

async function loadTemplates() {
  const body = await api("/api/v1/templates");
  const grid = $("templates");
  grid.replaceChildren();
  $("templates-empty").hidden = body.templates.length > 0;

  for (const template of body.templates) {
    const card = el("div", "card");
    const head = el("div", "card-head");
    head.appendChild(el("h3", null, template.name));
    if (!template.buildable) {
      head.appendChild(el("span", "state", "layer only"));
    }
    card.appendChild(head);

    if (template.inherits) {
      card.appendChild(el("div", "sub", "inherits " + template.inherits));
    }

    const pins = Object.entries(template.artifacts || {});
    if (pins.length) {
      const table = el("table", "list");
      const tbody = document.createElement("tbody");
      for (const [name, pin] of pins) {
        const row = document.createElement("tr");
        row.appendChild(cell(name));
        row.appendChild(cell(pin, "right mono dim"));
        tbody.appendChild(row);
      }
      table.appendChild(tbody);
      card.appendChild(table);
    }

    if (template.buildable) {
      const actions = el("div", "actions");
      const build = el("button", null, "build");
      build.addEventListener("click", async () => {
        build.disabled = true;
        try {
          const result = await api(
            "/api/v1/templates/" + encodeURIComponent(template.name) + "/build",
            { method: "POST", body: {} }
          );
          toast("built " + result.build.template, "ok");
        } catch (e) {
          toast(e.message, "bad");
        } finally {
          build.disabled = false;
        }
      });
      actions.appendChild(build);
      card.appendChild(actions);
    }

    grid.appendChild(card);
  }
}

/* Pipeline */

$("run-pipeline").addEventListener("click", async () => {
  const button = $("run-pipeline");
  const output = $("pipeline-output");
  button.disabled = true;
  output.hidden = false;
  output.textContent = "running...";
  try {
    const body = await api("/api/v1/pipeline/run", { method: "POST", body: {} });
    output.textContent = JSON.stringify(body.pipeline, null, 2);
    toast(body.pipeline.clean ? "pipeline finished" : "pipeline finished with problems",
      body.pipeline.clean ? "ok" : "bad");
  } catch (e) {
    output.textContent = e.message;
    toast(e.message, "bad");
  } finally {
    button.disabled = false;
  }
});

$("reload-config").addEventListener("click", async () => {
  try {
    const body = await api("/api/v1/config/reload", { method: "POST", body: {} });
    toast(body.artifacts + " artifacts, " + body.templates + " templates", "ok");
  } catch (e) {
    toast(e.message, "bad");
  }
});

/* Tokens */

async function loadTokens() {
  const body = await api("/api/v1/tokens");
  const rows = $("tokens-table").querySelector("tbody");
  rows.replaceChildren();
  for (const token of body.tokens) {
    const row = document.createElement("tr");
    row.appendChild(cell(token.name));
    row.appendChild(cell(token.role, "dim"));
    row.appendChild(cell(token.prefix + "...", "mono dim"));
    row.appendChild(cell(when(token.last_used_at) || "never used", "dim"));

    const right = document.createElement("td");
    right.className = "right";
    if (token.revoked_at) {
      right.appendChild(el("span", "pill denied", "revoked"));
    } else {
      const revoke = el("button", "ghost", "revoke");
      revoke.addEventListener("click", async () => {
        if (!confirm("Revoke " + token.name + "? Whatever is using it stops working.")) return;
        try {
          await api("/api/v1/tokens/" + encodeURIComponent(token.id), { method: "DELETE" });
          loadTokens();
        } catch (e) {
          toast(e.message, "bad");
        }
      });
      right.appendChild(revoke);
    }
    row.appendChild(right);
    rows.appendChild(row);
  }
}

$("token-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  try {
    const body = await api("/api/v1/tokens", {
      method: "POST",
      body: { name: $("token-name").value, role: $("token-role").value },
    });
    const banner = $("new-token");
    banner.replaceChildren();
    banner.appendChild(text("Copy this now, it is not stored anywhere you can read it again: "));
    banner.appendChild(el("span", "mono", body.secret));
    banner.hidden = false;
    $("token-name").value = "";
    loadTokens();
  } catch (e) {
    toast(e.message, "bad");
  }
});

/* Audit */

async function loadAudit() {
  const body = await api("/api/v1/audit?limit=200");
  const rows = $("audit-table").querySelector("tbody");
  rows.replaceChildren();
  $("audit-empty").hidden = body.audit.length > 0;
  for (const entry of body.audit) {
    const row = document.createElement("tr");
    row.appendChild(cell(when(entry.at), "dim"));
    row.appendChild(cell(entry.actor));
    row.appendChild(cell(entry.surface, "dim"));
    row.appendChild(cell(entry.action + (entry.target ? " " + entry.target : ""), "mono"));
    const decision = document.createElement("td");
    decision.className = "right";
    decision.appendChild(el("span", "pill " + entry.decision, entry.decision));
    row.appendChild(decision);
    rows.appendChild(row);

    if (entry.decision === "denied" && entry.detail) {
      const why = document.createElement("tr");
      const spread = document.createElement("td");
      spread.colSpan = 5;
      spread.className = "dim";
      spread.appendChild(text(entry.detail));
      why.appendChild(spread);
      rows.appendChild(why);
    }
  }
}

function when(value) {
  if (!value) return "";
  const at = new Date(value);
  return Number.isNaN(at.getTime()) ? "" : at.toLocaleString();
}

/* Start */

(function start() {
  let saved = null;
  try {
    saved = localStorage.getItem(TOKEN_KEY);
  } catch (e) {
    saved = null;
  }
  if (!saved) {
    signOut(null);
    return;
  }
  signIn(saved).catch(() => signOut(null));
})();
