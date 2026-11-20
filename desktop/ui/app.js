// podbag's window: lists of containers, images, volumes and networks, their details,
// logs and actions, and the settings. Plain DOM, no framework. Data from the engine is
// always set as text, never as HTML.
"use strict";

const KINDS = ["containers", "images", "volumes", "networks"];
const t = (key, vars) => I18N.t(key, vars);

const S = {
  view: "containers", // a kind, or "settings"
  detail: null, // { kind, id, tab }
  lists: { containers: [], images: [], volumes: [], networks: [] },
  loaded: false,
  search: { containers: "", images: "", volumes: "", networks: "" },
  engine: null,
  live: false,
  features: { stats: false, top: false, pause: false, rename: false },
  featuresAsked: false,
  settings: { language: "system", theme: "system" },
  busy: new Set(),
  logs: { generation: 0, lines: [], follow: true },
  languageNames: null,
};

// ---- small helpers ----

function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

// An icon-only button, named by its label (also its tooltip).
function iconButton(name, label, onclick, cls) {
  return h("button", { type: "button", class: `icon-button ${cls || ""}`, "aria-label": label, title: label, onclick }, icon(name));
}

function button(name, label, onclick, cls) {
  return h("button", { type: "button", class: `button ${cls || ""}`, onclick }, name && icon(name), h("span", { text: label }));
}

const shortId = (id) => String(id || "").replace(/^sha256:/, "").slice(0, 12);
const containerName = (c) => (c.Names && c.Names[0] ? c.Names[0].replace(/^\//, "") : shortId(c.Id));
const imageName = (i) => (i.RepoTags || []).find((x) => x !== "<none>:<none>") || shortId(i.Id);

function ago(when) {
  const secs = typeof when === "number" ? when : Date.parse(when) / 1000;
  if (!secs || Number.isNaN(secs)) return "";
  const diff = secs - Date.now() / 1000;
  const rtf = new Intl.RelativeTimeFormat(I18N.lang, { numeric: "auto", style: "long" });
  const units = [["year", 31536000], ["month", 2592000], ["week", 604800], ["day", 86400], ["hour", 3600], ["minute", 60]];
  for (const [unit, size] of units) {
    if (Math.abs(diff) >= size) return rtf.format(Math.round(diff / size), unit);
  }
  return rtf.format(0, "second");
}

function size(bytes) {
  const units = [["gigabyte", 1e9], ["megabyte", 1e6], ["kilobyte", 1e3]];
  for (const [unit, n] of units) {
    if (bytes >= n) {
      return new Intl.NumberFormat(I18N.lang, { style: "unit", unit, unitDisplay: "short", maximumFractionDigits: 1 }).format(bytes / n);
    }
  }
  return new Intl.NumberFormat(I18N.lang, { style: "unit", unit: "byte", unitDisplay: "short" }).format(bytes || 0);
}

function portsText(c) {
  const seen = new Set();
  for (const p of c.Ports || []) {
    const type = p.Type || "tcp";
    seen.add(p.PublicPort ? `${p.PublicPort}→${p.PrivatePort}/${type}` : `${p.PrivatePort}/${type}`);
  }
  return [...seen].join(", ");
}

function stateChip(state) {
  const known = ["running", "exited", "created", "paused", "restarting", "dead"];
  const s = known.includes(state) ? state : "created";
  return h("span", { class: `chip chip-${s}` }, h("span", { class: "dot", "aria-hidden": "true" }), t(`state.${s}`));
}

function toast(text, error) {
  const el = h("div", { class: `toast ${error ? "toast-error" : ""}`, text });
  document.getElementById("toasts").append(el);
  setTimeout(() => el.remove(), error ? 8000 : 4000);
}

async function copy(text, btn) {
  try {
    await navigator.clipboard.writeText(text);
    if (btn) {
      btn.setAttribute("title", t("action.copied"));
      btn.setAttribute("aria-label", t("action.copied"));
    }
    toast(t("action.copied"));
  } catch (e) {
    toast(String(e), true);
  }
}

function codeLine(text) {
  return h("div", { class: "code" }, h("code", { text }), iconButton("copy", t("action.copy"), (e) => copy(text, e.currentTarget)));
}

// ---- dialogs ----

function confirmDialog(text, yesLabel) {
  const d = document.getElementById("confirm");
  document.getElementById("confirm-title").textContent = t("confirm.title");
  document.getElementById("confirm-text").textContent = text;
  document.getElementById("confirm-cancel").textContent = t("action.cancel");
  document.getElementById("confirm-yes").textContent = yesLabel || t("action.remove");
  d.returnValue = "";
  d.showModal();
  return new Promise((resolve) => {
    d.addEventListener("close", () => {
      const yes = d.returnValue === "yes";
      d.returnValue = "";
      resolve(yes);
    }, { once: true });
  });
}

// A dialog that asks for one line of text, then runs work(text) while it stays open.
function askDialog({ title, label, placeholder, value, okLabel, work }) {
  const d = document.getElementById("ask");
  const form = document.getElementById("ask-form");
  const input = document.getElementById("ask-input");
  const progress = document.getElementById("ask-progress");
  const ok = document.getElementById("ask-ok");
  document.getElementById("ask-title").textContent = title;
  document.getElementById("ask-label").textContent = label;
  document.getElementById("ask-cancel").textContent = t("action.cancel");
  ok.textContent = okLabel;
  input.placeholder = placeholder || "";
  input.value = value || "";
  input.disabled = false;
  ok.disabled = false;
  progress.textContent = "";
  d.showModal();
  input.select();
  const onSubmit = async (e) => {
    if (e.submitter && e.submitter.value !== "ok") return;
    e.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    input.disabled = true;
    ok.disabled = true;
    try {
      await work(text, (line) => { progress.textContent = line; });
      d.close();
    } catch (err) {
      progress.textContent = String(err);
      input.disabled = false;
      ok.disabled = false;
    }
  };
  form.addEventListener("submit", onSubmit);
  d.addEventListener("close", () => form.removeEventListener("submit", onSubmit), { once: true });
}

// ---- engine and data ----

async function connect(startVm) {
  S.engine = null;
  renderEngine();
  try {
    S.engine = await API.engine(startVm);
  } catch (e) {
    S.engine = { reachable: false, error: String(e), platform: "linux", endpoint: "" };
  }
  renderEngine();
  if (S.engine.reachable) {
    await refresh();
    API.watch(true);
  } else {
    S.loaded = false;
    renderMain();
  }
}

async function refresh() {
  try {
    const lists = await Promise.all(KINDS.map((k) => API.list(k)));
    KINDS.forEach((k, i) => { S.lists[k] = lists[i] || []; });
    S.loaded = true;
  } catch (e) {
    S.engine = { ...(S.engine || {}), reachable: false, error: String(e) };
    renderEngine();
  }
  if (!S.featuresAsked && S.loaded) {
    S.featuresAsked = true;
    const sample = S.lists.containers.find((c) => c.State === "running");
    API.features(sample && sample.Id).then((f) => { S.features = f; renderMain(); }).catch(() => {});
  }
  renderNav();
  renderMain();
}

let refreshTimer = null;
function refreshSoon() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, 200);
}

async function act(c, action) {
  const name = containerName(c);
  S.busy.add(c.Id);
  renderMain();
  try {
    await API.act(c.Id, action);
    toast(t(`done.${action}`, { name }));
  } catch (e) {
    toast(String(e), true);
  }
  S.busy.delete(c.Id);
  refreshSoon();
}

function objectName(kind, o) {
  if (kind === "containers") return containerName(o);
  if (kind === "images") return imageName(o);
  return o.Name;
}

function objectId(kind, o) {
  return kind === "volumes" ? o.Name : o.Id;
}

async function remove(kind, o) {
  const name = objectName(kind, o);
  const running = kind === "containers" && (o.State === "running" || o.State === "paused" || o.State === "restarting");
  const key = {
    containers: running ? "confirm.removeRunning" : "confirm.removeContainer",
    images: "confirm.removeImage",
    volumes: "confirm.removeVolume",
    networks: "confirm.removeNetwork",
  }[kind];
  if (!(await confirmDialog(t(key, { name })))) return;
  const id = objectId(kind, o);
  S.busy.add(id);
  renderMain();
  try {
    await API.remove(kind, id, running);
    toast(t("done.remove", { name }));
    if (S.detail && S.detail.id === id) closeDetail();
  } catch (e) {
    toast(String(e), true);
  }
  S.busy.delete(id);
  refreshSoon();
}

async function prune(kind) {
  const key = { containers: "confirm.pruneContainers", images: "confirm.pruneImages", volumes: "confirm.pruneVolumes", networks: "confirm.pruneNetworks" }[kind];
  if (!(await confirmDialog(t(key), t("action.prune")))) return;
  try {
    const p = await API.prune(kind);
    toast(t("done.prune", { n: p.count, size: size(p.bytes) }));
  } catch (e) {
    toast(String(e), true);
  }
  refreshSoon();
}

let pullLine = null;
function pull() {
  askDialog({
    title: t("pull.title"),
    label: t("pull.label"),
    placeholder: t("pull.placeholder"),
    okLabel: t("action.pull"),
    work: async (image, progress) => {
      pullLine = progress;
      progress(t("pull.working"));
      try {
        await API.pull(image);
      } finally {
        pullLine = null;
      }
      toast(t("done.pull", { name: image }));
      refreshSoon();
    },
  });
}

function rename(c) {
  askDialog({
    title: t("rename.title"),
    label: t("rename.label"),
    value: containerName(c),
    okLabel: t("action.rename"),
    work: async (name) => {
      await API.rename(c.Id, name);
      toast(t("done.rename", { name }));
      refreshSoon();
    },
  });
}

async function shell(c) {
  try {
    await API.openShell(c.Id);
  } catch (command) {
    const d = document.getElementById("confirm");
    document.getElementById("confirm-title").textContent = t("action.shell");
    document.getElementById("confirm-text").textContent = `${t("shell.manual")}\n\n${command}`;
    document.getElementById("confirm-cancel").textContent = t("action.back");
    const yes = document.getElementById("confirm-yes");
    yes.textContent = t("action.copy");
    d.showModal();
    d.addEventListener("close", () => {
      if (d.returnValue === "yes") copy(String(command));
      d.returnValue = "";
    }, { once: true });
  }
}

// ---- the macOS window toolbar ----

const windowBar = window.podbagWindowToolbar && document.getElementById("window-bar");

function setupWindowBar() {
  if (!windowBar) return;
  const actions = document.getElementById("window-actions");
  windowBar.prepend(document.querySelector(".brand"));
  document.getElementById("engine").remove();
  const reload = iconButton("refresh", "", () => S.engine?.reachable ? refresh() : connect(false));
  const settings = iconButton("settings", "", () => go("settings"));
  actions.append(reload, settings);
  document.getElementById("nav-bottom").remove();
  windowBar.hidden = false;

  // WebKit versions without focusgroup need one tab stop and arrow-key navigation.
  if (!("focusGroup" in HTMLElement.prototype)) {
    settings.tabIndex = -1;
    actions.addEventListener("focusin", ({ target }) => {
      reload.tabIndex = target === reload ? 0 : -1;
      settings.tabIndex = target === settings ? 0 : -1;
    });
    actions.addEventListener("keydown", (event) => {
      const { key, target } = event;
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(key)) return;
      event.preventDefault();
      if (key === "Home") reload.focus();
      else if (key === "End") settings.focus();
      else (target === reload ? settings : reload).focus();
    });
  }
}

// ---- the sidebar ----

function go(view) {
  if (S.detail) closeDetail();
  S.view = view;
  renderNav();
  renderMain(true);
}

function renderNav() {
  const nav = document.getElementById("nav");
  nav.replaceChildren(...KINDS.map((k) =>
    h("li", {}, h("button", {
      type: "button", class: "nav-item", "aria-current": S.view === k ? "page" : null, onclick: () => go(k),
    }, icon(k), h("span", { class: "label", text: t(`nav.${k}`) }), S.loaded ? h("span", { class: "count", text: S.lists[k].length }) : null))));
  if (windowBar) {
    const [reload, settings] = document.getElementById("window-actions").children;
    for (const [button, label] of [[reload, t("action.refresh")], [settings, t("nav.settings")]]) {
      button.title = label;
      button.setAttribute("aria-label", label);
    }
    if (S.view === "settings") settings.setAttribute("aria-current", "page");
    else settings.removeAttribute("aria-current");
    return;
  }
  document.getElementById("nav-bottom").replaceChildren(
    h("li", {}, h("button", {
      type: "button", class: "nav-item", "aria-current": S.view === "settings" ? "page" : null, onclick: () => go("settings"),
    }, icon("settings"), h("span", { class: "label", text: t("nav.settings") }))));
  document.querySelector(".sidebar").setAttribute("aria-label", "podbag");
}

function engineState() {
  const e = S.engine;
  if (!e) return ["checking", t("engine.checking")];
  if (e.vm === "running") return ["ok", t("engine.vmRunning")];
  if (e.vm && !e.reachable) return ["off", t("engine.vmStopped")];
  if (e.reachable) return ["ok", t("engine.running")];
  return ["down", t("engine.down")];
}

function renderEngine() {
  syncVmSettings();
  const el = document.getElementById("engine");
  if (!el) return;
  const [cls, text] = engineState();
  el.className = `engine engine-${cls}`;
  el.title = (S.engine && (S.engine.version || S.engine.error)) || "";
  el.replaceChildren(h("span", { class: "dot", "aria-hidden": "true" }), h("span", { text }));
}

// ---- the main area ----

let shownView = null;

function renderMain(fresh) {
  const main = document.getElementById("main");
  if (S.view === "settings") {
    if (fresh || shownView !== "settings") renderSettings(main);
    shownView = "settings";
    return;
  }
  if (S.engine && !S.engine.reachable) {
    shownView = "offline";
    main.replaceChildren(offline());
    return;
  }
  if (S.detail) {
    renderDetail(main, fresh);
    return;
  }
  const key = `list:${S.view}`;
  if (fresh || shownView !== key) {
    main.replaceChildren(toolbar(S.view), h("div", { class: "content", id: "content" }));
    shownView = key;
  }
  renderList();
}

function toolbar(kind) {
  const search = h("input", {
    type: "search", class: "search", placeholder: t("search.placeholder"), "aria-label": t("search.placeholder"),
    value: S.search[kind],
    oninput: (e) => { S.search[kind] = e.target.value; renderList(); },
  });
  search.value = S.search[kind];
  return h("header", { class: "toolbar" },
    h("h1", { text: t(`nav.${kind}`) }),
    h("label", { class: "search-box" }, icon("search"), search),
    h("div", { class: "tools" },
      h("span", { class: `live ${S.live ? "live-on" : ""}`, id: "live", title: S.live ? t("live.on") : t("live.off") },
        h("span", { class: "dot", "aria-hidden": "true" }), h("span", { class: "live-text", text: S.live ? t("live.on") : "" })),
      kind === "images" ? button("pull", t("action.pull"), pull, "primary") : null,
      button("prune", t("action.prune"), () => prune(kind)),
      windowBar ? null : iconButton("refresh", t("action.refresh"), refresh)));
}

function renderLive() {
  const el = document.getElementById("live");
  if (!el) return;
  el.classList.toggle("live-on", S.live);
  el.title = S.live ? t("live.on") : t("live.off");
  el.querySelector(".live-text").textContent = S.live ? t("live.on") : "";
}

const COLUMNS = {
  containers: [["col.name", "name"], ["col.image", "image"], ["col.state", "state"], ["col.ports", "ports"], ["col.created", "created"]],
  images: [["col.name", "name"], ["col.id", "id"], ["col.size", "size"], ["col.created", "created"]],
  volumes: [["col.name", "name"], ["col.driver", "driver"], ["col.created", "created"]],
  networks: [["col.name", "name"], ["col.driver", "driver"], ["col.subnet", "subnet"], ["col.id", "id"]],
};

function cells(kind, o) {
  switch (kind) {
    case "containers":
      return [
        h("div", { class: "name" }, h("span", { class: "primary-text", text: containerName(o) }), h("span", { class: "secondary-text mono", text: shortId(o.Id) })),
        h("span", { class: "truncate", text: o.Image, title: o.Image }),
        stateChip(o.State),
        h("span", { class: "mono truncate", text: portsText(o) || "—" }),
        h("span", { class: "secondary-text", text: ago(o.Created) }),
      ];
    case "images":
      return [
        h("span", { class: "primary-text truncate", text: imageName(o), title: imageName(o) }),
        h("span", { class: "mono secondary-text", text: shortId(o.Id) }),
        h("span", { text: size(o.Size) }),
        h("span", { class: "secondary-text", text: ago(o.Created) }),
      ];
    case "volumes":
      return [
        h("span", { class: "primary-text truncate", text: o.Name, title: o.Name }),
        h("span", { text: o.Driver }),
        h("span", { class: "secondary-text", text: ago(o.CreatedAt) }),
      ];
    default:
      return [
        h("div", { class: "name" }, h("span", { class: "primary-text", text: o.Name }), builtinNetwork(o) ? h("span", { class: "tag", text: t("state.builtin") }) : null),
        h("span", { text: o.Driver }),
        h("span", { class: "mono", text: subnet(o) || "—" }),
        h("span", { class: "mono secondary-text", text: shortId(o.Id) }),
      ];
  }
}

const builtinNetwork = (n) => ["bridge", "host", "none"].includes(n.Name);
const subnet = (n) => (((n.IPAM || {}).Config || [])[0] || {}).Subnet || "";

function rowActions(kind, o) {
  const busy = S.busy.has(objectId(kind, o));
  const out = [];
  if (kind === "containers") {
    const st = o.State;
    if (st === "running" || st === "restarting") {
      out.push(iconButton("stop", t("action.stop"), () => act(o, "stop")));
      out.push(iconButton("restart", t("action.restart"), () => act(o, "restart")));
      if (S.features.pause) out.push(iconButton("pause", t("action.pause"), () => act(o, "pause")));
    } else if (st === "paused") {
      if (S.features.pause) out.push(iconButton("start", t("action.resume"), () => act(o, "unpause")));
      out.push(iconButton("stop", t("action.stop"), () => act(o, "stop")));
    } else {
      out.push(iconButton("start", t("action.start"), () => act(o, "start"), "accent"));
    }
    out.push(iconButton("logs", t("action.logs"), () => openDetail(kind, o.Id, "logs")));
    if (st === "running") out.push(iconButton("shell", t("action.shell"), () => shell(o)));
  }
  if (!(kind === "networks" && builtinNetwork(o))) {
    out.push(iconButton("remove", t("action.remove"), () => remove(kind, o), "danger"));
  }
  const box = h("div", { class: "actions", role: "group", "aria-label": t("col.actions") }, out);
  if (busy) box.querySelectorAll("button").forEach((b) => { b.disabled = true; });
  return box;
}

function matches(kind, o, q) {
  if (!q) return true;
  q = q.toLowerCase();
  const fields = kind === "containers" ? [containerName(o), o.Image, o.Id, o.State, portsText(o)]
    : kind === "images" ? [...(o.RepoTags || []), o.Id]
      : kind === "volumes" ? [o.Name, o.Driver]
        : [o.Name, o.Driver, o.Id, subnet(o)];
  return fields.some((f) => String(f || "").toLowerCase().includes(q));
}

function renderList() {
  const content = document.getElementById("content");
  if (!content) return;
  const kind = S.view;
  if (!S.loaded) {
    content.replaceChildren(h("p", { class: "loading", text: t("engine.checking") }));
    return;
  }
  const all = S.lists[kind];
  const q = S.search[kind].trim();
  const rows = all.filter((o) => matches(kind, o, q));
  if (!all.length) {
    content.replaceChildren(emptyState(kind));
    return;
  }
  if (!rows.length) {
    content.replaceChildren(h("p", { class: "nothing", text: t("empty.search", { q }) }));
    return;
  }
  const cols = COLUMNS[kind];
  const head = h("thead", {}, h("tr", {}, cols.map(([key, c]) => h("th", { class: `col-${c}`, scope: "col", text: t(key) })),
    h("th", { class: "col-actions", scope: "col" }, h("span", { class: "visually-hidden", text: t("col.actions") }))));
  const body = h("tbody", {}, rows.map((o) => {
    const tr = h("tr", {}, cells(kind, o).map((cell, i) => h("td", { class: `col-${cols[i][1]}` }, cell)), h("td", { class: "col-actions" }, rowActions(kind, o)));
    // The name opens the details; the row is clickable too, for the mouse.
    const first = tr.querySelector("td .primary-text");
    if (first) {
      const link = h("button", { type: "button", class: "link", onclick: () => openDetail(kind, objectId(kind, o), "info") });
      first.replaceWith(link);
      link.append(first);
    }
    tr.addEventListener("click", (e) => {
      if (e.target.closest("button")) return;
      openDetail(kind, objectId(kind, o), "info");
    });
    return tr;
  }));
  content.replaceChildren(h("table", { class: `list list-${kind}` }, head, body));
}

function emptyState(kind) {
  const commands = {
    containers: "podbag run -d --name web -p 8080:80 nginx:alpine",
    volumes: "podbag volume create data",
    networks: "podbag network create app",
  };
  return h("div", { class: "empty" },
    h("div", { class: "empty-icon" }, icon(kind)),
    h("h2", { text: t(`empty.${kind}.title`) }),
    h("p", { text: t(`empty.${kind}.text`) }),
    commands[kind] ? codeLine(commands[kind]) : null,
    kind === "images" ? button("pull", t("action.pull"), pull, "primary") : null);
}

function offline() {
  const e = S.engine || {};
  const vmOff = e.error === "vm-stopped" || (e.vm && e.vm !== "running");
  if (vmOff) {
    return h("div", { class: "empty" },
      h("img", { src: "logo-small.png", alt: "", width: 72, height: 72 }),
      h("h2", { text: t("offline.vmTitle") }),
      h("p", { text: t("offline.vmText") }),
      button("start", t("action.startVm"), (ev) => { ev.currentTarget.disabled = true; connect(true); }, "primary"));
  }
  const hint = e.platform === "linux" ? "podbag system service" : "podbag vm start";
  return h("div", { class: "empty" },
    h("img", { src: "logo-small.png", alt: "", width: 72, height: 72 }),
    h("h2", { text: t("offline.title") }),
    h("p", { text: t("offline.text") }),
    e.error && e.error !== "vm-stopped" ? h("p", { class: "error-text mono", text: e.error }) : null,
    h("p", { text: t("offline.hint") }),
    codeLine(hint),
    button("refresh", t("action.retry"), () => connect(false), "primary"));
}

// ---- details ----

function findObject(kind, id) {
  return S.lists[kind].find((o) => objectId(kind, o) === id || o.Id === id);
}

function openDetail(kind, id, tab) {
  S.view = kind;
  S.detail = { kind, id, tab, json: null, extra: null };
  renderNav();
  renderMain(true);
}

function closeDetail() {
  if (S.detail && S.detail.tab === "logs") API.logsStop();
  S.detail = null;
  renderMain(true);
}

function renderDetail(main, fresh) {
  const d = S.detail;
  const o = findObject(d.kind, d.id);
  const key = `detail:${d.kind}:${d.id}:${d.tab}`;
  if (!o) {
    closeDetail();
    return;
  }
  const name = objectName(d.kind, o);
  const tabs = d.kind === "containers" ? ["info", "logs", "inspect"] : ["info", "inspect"];
  const actions = d.kind === "containers" ? [
    ...[...rowActions(d.kind, o).children].filter((b) => b.getAttribute("aria-label") !== t("action.logs")),
    S.features.rename ? iconButton("rename", t("action.rename"), () => rename(o)) : null,
  ] : [...rowActions(d.kind, o).children];
  const header = h("header", { class: "toolbar" },
    iconButton("back", t("action.back"), closeDetail),
    h("h1", { class: "truncate", text: name, title: name }),
    d.kind === "containers" ? stateChip(o.State) : null,
    h("div", { class: "tools actions" }, actions));
  const tablist = h("div", { class: "tabs", role: "tablist" }, tabs.map((tab) => h("button", {
    type: "button", role: "tab", class: "tab", "aria-selected": d.tab === tab ? "true" : "false",
    onclick: () => { if (d.tab === "logs" && tab !== "logs") API.logsStop(); d.tab = tab; renderMain(true); },
  }, t(`detail.${tab}`))));
  if (!fresh && shownView === key) {
    // Only the header and the info change on a refresh; logs and inspect stay.
    main.querySelector(".toolbar").replaceWith(header);
    if (d.tab === "info") main.querySelector(".panel").replaceWith(infoPanel(d.kind, o));
    return;
  }
  shownView = key;
  let panel;
  if (d.tab === "info") panel = infoPanel(d.kind, o);
  else if (d.tab === "logs") panel = logsPanel(o);
  else panel = inspectPanel(d.kind, d.id);
  main.replaceChildren(header, tablist, panel);
}

function field(label, value, cls) {
  return [h("dt", { text: label }), h("dd", { class: cls || "", text: value == null || value === "" ? t("detail.none") : value })];
}

function infoPanel(kind, o) {
  let rows;
  if (kind === "containers") {
    rows = [
      field(t("col.id"), o.Id, "mono"),
      field(t("col.image"), o.Image),
      field(t("detail.command"), o.Command, "mono"),
      field(t("detail.status"), o.Status),
      field(t("col.ports"), portsText(o)),
      field(t("col.created"), ago(o.Created)),
    ];
  } else if (kind === "images") {
    const users = S.lists.containers.filter((c) => (o.RepoTags || []).includes(c.Image) || c.Image === o.Id).map(containerName);
    rows = [
      field(t("col.id"), o.Id, "mono"),
      field(t("detail.tags"), (o.RepoTags || []).join(", ")),
      field(t("col.size"), size(o.Size)),
      field(t("col.created"), ago(o.Created)),
      field(t("detail.used"), users.join(", ")),
    ];
  } else if (kind === "volumes") {
    rows = [
      field(t("col.name"), o.Name),
      field(t("col.driver"), o.Driver),
      field(t("detail.path"), o.Mountpoint, "mono"),
      field(t("col.created"), ago(o.CreatedAt)),
    ];
  } else {
    const cfg = ((o.IPAM || {}).Config || [])[0] || {};
    rows = [
      field(t("col.id"), o.Id, "mono"),
      field(t("col.driver"), o.Driver),
      field(t("col.subnet"), cfg.Subnet, "mono"),
      field(t("detail.gateway"), cfg.Gateway, "mono"),
      field(t("detail.scope"), o.Scope),
    ];
  }
  const panel = h("div", { class: "panel" }, h("dl", { class: "fields" }, rows));
  if (kind === "containers" && o.State === "running") {
    for (const [on, what, label] of [[S.features.top, "top", "detail.processes"], [S.features.stats, "stats", "detail.usage"]]) {
      if (!on) continue;
      const pre = h("pre", { class: "json" });
      panel.append(h("h2", { class: "panel-title", text: t(label) }), pre);
      API.containerJson(o.Id, what).then((v) => { pre.textContent = JSON.stringify(v, null, 2); }).catch((e) => { pre.textContent = String(e); });
    }
  }
  return panel;
}

function inspectPanel(kind, id) {
  const pre = h("pre", { class: "json", tabindex: "0" });
  API.inspect(kind, id).then((v) => { pre.textContent = JSON.stringify(v, null, 2); }).catch((e) => { pre.textContent = String(e); });
  return h("div", { class: "panel panel-fill" }, pre);
}

function logsPanel(c) {
  const pre = h("pre", { class: "logs", id: "logs", tabindex: "0" }, h("span", { class: "secondary-text", text: t("detail.noLogs") }));
  const follow = h("input", { type: "checkbox", id: "follow", checked: S.logs.follow, onchange: (e) => { S.logs.follow = e.target.checked; scrollLogs(); } });
  S.logs.lines = [];
  API.logsStart(c.Id).then((g) => { S.logs.generation = g; }).catch((e) => { pre.textContent = String(e); });
  return h("div", { class: "panel panel-fill" },
    h("div", { class: "panel-bar" }, h("label", { class: "check" }, follow, h("span", { text: t("action.follow") }))),
    pre);
}

function scrollLogs() {
  const pre = document.getElementById("logs");
  if (pre && S.logs.follow) pre.scrollTop = pre.scrollHeight;
}

function onLogs(batch) {
  if (batch.generation !== S.logs.generation && S.logs.generation !== 0) return;
  const pre = document.getElementById("logs");
  if (!pre) return;
  if (!S.logs.lines.length) pre.textContent = "";
  S.logs.lines.push(...batch.lines);
  pre.append(document.createTextNode(batch.lines.join("\n") + "\n"));
  // Keep the last 5000 lines.
  if (S.logs.lines.length > 6000) {
    S.logs.lines = S.logs.lines.slice(-5000);
    pre.textContent = S.logs.lines.join("\n") + "\n";
  }
  scrollLogs();
}

// ---- settings ----

let vmActionPending = false;

function syncVmSettings() {
  const card = document.getElementById("vm-settings");
  if (!card) return;
  if (S.engine) card.hidden = S.engine.platform !== "macos" || !S.engine.vm;
  if (card.hidden) return;
  if (!card.childElementCount) {
    const form = h("div", {});
    const toggle = button(null, "", async () => {
      if (vmActionPending || !S.engine) return;
      const action = S.engine.vm === "running" ? "stop" : "start";
      vmActionPending = true;
      syncVmSettings();
      try {
        await API.vm(action);
      } catch (err) {
        toast(String(err), true);
      } finally {
        vmActionPending = false;
        syncVmSettings();
      }
    });
    toggle.id = "vm-toggle";
    card.append(
      h("div", { class: "vm-row" },
        h("div", {}, h("h2", { class: "setting-label", text: "Linux VM" }),
          h("p", { id: "vm-status", role: "status" })), toggle), form);
    loadVmResources(form);
  }
  const vm = S.engine?.vm;
  const toggle = card.querySelector("#vm-toggle");
  toggle.disabled = !vm || vmActionPending;
  toggle.textContent = t(vm === "running" ? "tray.stopVm" : "tray.startVm");
  card.querySelector("#vm-status").textContent = vm ? t(vm === "running" ? "engine.vmRunning" : "engine.vmStopped") : t("engine.checking");
}

async function loadVmResources(form) {
  try {
    const { values, limits, envOverride } = await API.vmResources();
    let saved = values;
    const draft = { ...saved };
    const save = button(null, t("action.save"), async () => {
      fields.disabled = true;
      try {
        saved = (await API.vmResources(draft)).values;
        toast(t("vm.saved"));
      } catch (err) {
        toast(String(err), true);
      } finally {
        fields.disabled = false;
        updateSave();
      }
    }, "primary");
    function updateSave() {
      save.disabled = draft.cpus === saved.cpus && draft.memoryMib === saved.memoryMib;
    }
    function slider(key, label, min, max, step, format) {
      const id = "vm-" + key;
      const value = h("output", { for: id, text: format(draft[key]) });
      const input = h("input", {
        id, type: "range", min, max, step, value: draft[key], "aria-valuetext": value.textContent,
        oninput: () => {
          draft[key] = input.valueAsNumber;
          value.textContent = format(draft[key]);
          input.setAttribute("aria-valuetext", value.textContent);
          updateSave();
        },
      });
      return h("div", { class: "vm-slider" }, h("label", { for: id, text: label }), input, value);
    }
    const fields = h("fieldset", {}, h("legend", { class: "visually-hidden", text: t("detail.usage") }),
      slider("cpus", "CPU", limits.minCpus, limits.maxCpus, 1, (n) => n.toLocaleString(I18N.lang)),
      slider("memoryMib", t("vm.memory"), limits.minMemoryMib, limits.maxMemoryMib, limits.memoryStepMib,
        (n) => n < 1024 ? n + " MiB" : (n / 1024).toLocaleString(I18N.lang) + " GiB"),
      h("div", { class: "vm-row vm-footer" }, h("p", { text: t(envOverride ? "vm.envOverride" : "vm.nextBoot") }), save));
    updateSave();
    form.replaceChildren(fields);
  } catch (err) {
    form.replaceChildren(h("p", { class: "error-text", text: String(err) }),
      button(null, t("action.retry"), () => loadVmResources(form)));
  }
}

async function renderSettings(main) {
  if (!S.languageNames) S.languageNames = await I18N.names();
  if (S.view !== "settings") return;
  const lang = h("select", {
    id: "language",
    onchange: async (e) => { S.settings.language = e.target.value; await saveSettings(); },
  }, h("option", { value: "system", text: t("settings.system") }),
  I18N.LANGUAGES.map((l) => h("option", { value: l, lang: l, text: S.languageNames[l] })));
  lang.value = S.settings.language;
  const themes = ["system", "light", "dark"].map((v) => h("label", { class: "radio" },
    h("input", { type: "radio", name: "theme", value: v, checked: S.settings.theme === v, onchange: async () => { S.settings.theme = v; applyTheme(); await saveSettings(); } }),
    h("span", { text: t(`settings.${v}`) })));
  const e = S.engine || {};
  main.replaceChildren(
    h("header", { class: "toolbar" }, h("h1", { text: t("settings.title") })),
    h("div", { class: "content settings" },
      h("section", { id: "vm-settings", class: "card", hidden: true }),
      h("section", { class: "card" },
        h("label", { class: "setting-label", for: "language", text: t("settings.language") }), lang),
      h("section", { class: "card" },
        h("fieldset", {}, h("legend", { class: "setting-label", text: t("settings.theme") }), h("div", { class: "radios" }, themes))),
      h("section", { class: "card" },
        h("h2", { class: "setting-label", text: t("settings.engine") }),
        h("dl", { class: "fields" },
          field(t("settings.socket"), e.endpoint, "mono"),
          field(t("settings.version"), e.version))),
      h("section", { class: "card" },
        h("h2", { class: "setting-label", text: t("settings.about") }),
        h("p", { text: t("settings.aboutText") }))));
  syncVmSettings();
}

async function saveSettings() {
  try {
    await API.setSettings(S.settings);
  } catch (e) {
    toast(String(e), true);
  }
  await I18N.load(S.settings.language);
  renderNav();
  renderEngine();
  renderMain(true);
}

function applyTheme() {
  const v = S.settings.theme === "light" || S.settings.theme === "dark" ? S.settings.theme : "light dark";
  document.querySelector('meta[name="color-scheme"]').content = v;
}

// ---- start ----

let readyDone;
const ready = new Promise((r) => { readyDone = r; });

async function init() {
  try {
    S.settings = { ...S.settings, ...(await API.getSettings()) };
  } catch { /* the defaults */ }
  applyTheme();
  await I18N.load(S.settings.language);
  setupWindowBar();
  renderNav();
  renderEngine();
  renderMain(true);
  API.on("changed", refreshSoon);
  API.on("live", (v) => { S.live = !!v; renderLive(); });
  API.on("logs", onLogs);
  API.on("pull-progress", (line) => { if (pullLine) pullLine(line); });
  API.on("shown", () => connect(false));
  API.on("vm-status", (vm) => {
    if (S.engine?.platform !== "macos" || !S.engine.vm || S.engine.vm === vm) return;
    S.engine.vm = vm;
    if (vm === "stopped") {
      S.engine.reachable = false;
      S.loaded = false;
      clearTimeout(refreshTimer);
      API.watch(false);
      renderNav();
    } else if (!S.engine.reachable && !document.hidden && !vmActionPending) {
      connect(false);
      return;
    }
    renderEngine();
    renderMain();
  });
  API.on("engine-changed", () => connect(false));
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) return API.watch(false);
    if (S.engine && S.engine.reachable) { API.watch(true); refresh(); }
  });
  document.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "f") {
      const s = document.querySelector(".search");
      if (s) { e.preventDefault(); s.focus(); }
    } else if (e.key === "Escape" && S.detail && !document.querySelector("dialog[open]")) {
      closeDetail();
    }
  });
  // Without events, read the lists every few seconds while the window shows.
  setInterval(() => {
    if (!S.live && !document.hidden && S.engine && S.engine.reachable) refresh();
  }, 3000);
  await connect(false);
  readyDone();
}

// For the preview page (desktop/preview), which shows each view with a mocked API.
window.podbag = { ready, go, openDetail, remove: (kind, i) => remove(kind, S.lists[kind][i]), pull, prune, state: S };

init();
