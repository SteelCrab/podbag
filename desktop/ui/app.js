// podbag's window: lists of containers, images, volumes and networks, their details,
// logs and actions, and the settings. Plain DOM, no framework. Data from the engine is
// always set as text, never as HTML.
"use strict";

const KINDS = ["containers", "images", "volumes", "networks"];
const t = (key, vars) => I18N.t(key, vars);

const S = {
  view: "migration", // an engine resource, "kubernetes" or "settings"
  kubeKind: "pods",
  kubeCounts: {},
  kubeOpenShift: false, // the selected context serves Routes
  kubeAllNamespaces: false,
  kubeContext: sessionStorage.getItem("kube-context") || "",
  connection: null,
  contexts: [],
  listErrors: {},
  kubeExpanded: false,
  trainingExpanded: false,
  trainingRecords: [],
  detail: null, // { kind, id, tab }
  lists: { containers: [], images: [], volumes: [], networks: [] },
  loaded: false,
  search: { containers: "", images: "", volumes: "", networks: "" },
  engine: null,
  live: false,
  features: { stats: false, top: false, pause: false, rename: false },
  featuresAsked: false,
  settings: { language: "system", theme: "system", sidebarWidth: 216, sidebarCollapsed: false },
  busy: new Set(),
  logs: { generation: 0, lines: [], follow: true },
  languageNames: null,
  vm: null, // podbag vm status --json, or null
  updates: null, // versions and the last update check
  upgrade: null, // what the last "Update podbag" did
  history: { stack: [], index: -1 },
  docker: null, // where the docker command goes
};

const IS_MAC = /Mac/.test(navigator.platform || navigator.userAgent);
const SIDEBAR = { width: 216, min: 160, max: 360, rail: 56, collapseAt: 128, expandAt: 80 };

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

// Replace an element's children; null and false are left out.
function put(el, ...kids) {
  el.replaceChildren(...kids.flat(Infinity).filter((k) => k != null && k !== false));
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
  return h("button", { type: "button", class: `button ${cls || ""}`, onclick }, typeof name === "string" ? icon(name) : name, h("span", { text: label }));
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

// podbag's numbers for an image with compressed files (PodbagCompression in the engine's
// list): the size on disk, the plain size and the space saved in whole percent.
function compressionOf(image) {
  const c = image.PodbagCompression;
  if (!c || !(c.OriginalSize > 0)) return null;
  return { stored: c.Size, original: c.OriginalSize, saved: Math.max(0, Math.round((1 - c.Size / c.OriginalSize) * 100)) };
}

// The Size cell: one line, or for a compressed image the size on disk with the
// compression icon and, below it, how much smaller the image got.
function sizeCell(image) {
  const c = compressionOf(image);
  if (!c) return h("span", { text: size(image.Size) });
  const mark = icon("compress");
  mark.removeAttribute("aria-hidden");
  mark.setAttribute("role", "img");
  mark.setAttribute("aria-label", t("compression.compressed"));
  const note = t("compression.from", { size: size(c.original), percent: percent(c.saved) });
  return h("div", { class: "size-cell" },
    h("div", { class: "size-line" }, mark, h("span", { text: size(c.stored) })),
    h("div", { class: "size-note secondary-text", text: note, title: note }));
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

function confirmDialog(text, yesLabel, { title = t("confirm.title"), style = "danger" } = {}) {
  const d = document.getElementById("confirm");
  document.getElementById("confirm-title").textContent = title;
  document.getElementById("confirm-text").textContent = text;
  document.getElementById("confirm-cancel").textContent = t("action.cancel");
  const yes = document.getElementById("confirm-yes");
  yes.textContent = yesLabel || t("action.remove");
  yes.className = "button " + style;
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
  refreshVm();
  refreshDocker();
  if (S.engine.reachable) {
    await refresh();
    API.watch(true);
  } else {
    S.loaded = false;
    renderNav();
    renderMain();
  }
  renderStatusbar();
}

const isPodbag = () => !S.engine || S.engine.engine === "podbag";
const hasVm = () => isPodbag() && S.engine && S.engine.platform !== "linux";

// The VM's state, from files (it wakes nothing). Only for podbag on macOS and Windows.
async function refreshVm() {
  if (!hasVm()) {
    S.vm = null;
  } else {
    try {
      S.vm = await API.vmStatus();
    } catch (e) {
      S.vm = { state: "unknown", error: String(e) };
    }
  }
  renderVmCard();
  renderStatusbar();
  if (S.view === "settings") {
    renderVmSettings();
    if (S.vm?.state === "running" && document.querySelector("#compression-switch[data-waiting]")) readCompressionSwitch();
  }
}

// The Desktop and TUI share the preferred engine in Settings.
async function switchEngine(name) {
  if (S.connection.name === name) return;
  await API.setEngine(name);
  sessionStorage.setItem("engine-view", S.view);
  sessionStorage.setItem("engine-resource", S.kubeKind);
  location.reload();
}

async function refresh() {
  const results = await Promise.allSettled(KINDS.map((k) => API.list(k)));
  S.listErrors = {};
  results.forEach((result, i) => {
    const kind = KINDS[i];
    S.lists[kind] = result.status === "fulfilled" ? result.value : [];
    if (result.status === "rejected") S.listErrors[kind] = String(result.reason);
  });
  S.loaded = results.some((result) => result.status === "fulfilled");
  if (!S.loaded) {
    S.engine = { ...S.engine, reachable: false, error: S.listErrors.containers };
    renderEngine();
  }
  if (!S.featuresAsked && S.loaded && !S.listErrors.containers) {
    S.featuresAsked = true;
    const sample = S.lists.containers.find((c) => c.State === "running");
    API.features(sample && sample.Id).then((f) => { S.features = f; renderMain(); }).catch(() => {});
  }
  renderNav();
  renderMain();
  renderStatusbar();
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

// Pull progress goes here while a pull runs (the pull panel, the run form); images.js
// opens the pull panel.
let pullLine = null;
function pull() {}

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
  const actions = document.getElementById("topbar-tools");
  document.getElementById("engine").remove();
  const reload = iconButton("refresh", "", () => {
    if (S.view === "migration") return migrationPanel?.reload();
    if (S.view === "kubernetes") return kubePanel.reload();
    return S.engine?.reachable ? refresh() : connect(false);
  });
  const settings = iconButton("settings", "", () => go("settings"));
  actions.append(reload, settings);
  document.getElementById("nav-bottom").remove();
  windowBar.querySelector(".topbar-space").setAttribute("data-tauri-drag-region", "");
  windowBar.classList.add("native");
  windowBar.setAttribute("data-tauri-drag-region", "");

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

// Back and forward go through the pages seen (a list, a detail, the settings).
function remember() {
  const h = S.history;
  const entry = { view: S.view, detail: S.detail ? { kind: S.detail.kind, id: S.detail.id, tab: S.detail.tab } : null };
  if (S.view === "kubernetes") entry.kubeKind = S.kubeKind;
  const last = h.stack[h.index];
  if (last && JSON.stringify(last) === JSON.stringify(entry)) return;
  h.stack = h.stack.slice(0, h.index + 1);
  h.stack.push(entry);
  if (h.stack.length > 50) h.stack.shift();
  h.index = h.stack.length - 1;
  renderHistory();
}

function travel(step) {
  const h = S.history;
  const entry = h.stack[h.index + step];
  if (!entry) return;
  h.index += step;
  if (S.detail && S.detail.tab === "logs") API.logsStop();
  S.view = entry.view.startsWith("training:")
    && !trainingEntries().some(module => trainingView(module.id) === entry.view) ? "tutorials" : entry.view;
  if (S.view === "tutorials" || S.view.startsWith("training:")) S.trainingExpanded = true;
  if (entry.kubeKind) { S.kubeKind = kubeKinds().includes(entry.kubeKind) ? entry.kubeKind : "pods"; S.kubeExpanded = true; }
  S.detail = entry.detail ? { ...entry.detail, json: null, extra: null } : null;
  renderNav();
  renderMain(true);
  renderHistory();
}

function renderHistory() {
  const back = document.getElementById("nav-back");
  const forward = document.getElementById("nav-forward");
  const keys = IS_MAC ? ["⌘[", "⌘]"] : ["Alt+←", "Alt+→"];
  for (const [el, icn, key, shortcut, on] of [
    [back, "back", "action.back", keys[0], S.history.index > 0],
    [forward, "forward", "action.forward", keys[1], S.history.index < S.history.stack.length - 1],
  ]) {
    if (!el.firstChild) el.append(icon(icn));
    el.setAttribute("aria-label", t(key));
    el.title = `${t(key)} (${shortcut})`;
    el.disabled = !on;
  }
}

function go(view, kind = S.kubeKind) {
  if (view === "tutorials" || view.startsWith("training:")) S.trainingExpanded = true;
  if (S.detail) closeDetail(true);
  if (view === "kubernetes") {
    S.kubeExpanded = true;
    const changed = kind !== S.kubeKind;
    S.kubeKind = kind;
    if (S.view === view) {
      updateKubeNav();
      if (changed) { kubePanel.switchResource(); remember(); }
      return;
    }
  }
  S.view = view;
  renderNav();
  const rendered = renderMain(true);
  remember();
  return rendered;
}

// Keep the disclosure and child buttons mounted when the engine refreshes its lists.
function setupKubeNav() {
  const label = t("nav.kubernetes");
  const open = h("button", {
    type: "button", id: "nav-kube-open", class: "nav-item", "aria-label": label, title: label, onclick: () => go("kubernetes"),
  }, icon("kubernetes"), h("span", { class: "label", text: label }), h("span", { class: "count" }));
  const toggle = h("button", {
    type: "button", id: "nav-kube-toggle", class: "icon-button kube-toggle",
    "aria-label": t("kube.resources"), title: t("kube.resources"), "aria-controls": "nav-kube-children",
    onclick: () => { S.kubeExpanded = !S.kubeExpanded; updateKubeNav(); if (S.kubeExpanded) refreshKubeCounts(); },
  }, icon("back"));
  const children = h("ul", { class: "nav kube-subnav", id: "nav-kube-children", "aria-label": t("kube.resources") },
    KUBE_KINDS.map(kubeNavItem));
  document.getElementById("nav-kubernetes").replaceChildren(
    h("li", {}, h("div", { class: "kube-nav-header" }, open, toggle), children));
}

function kubeNavItem(kind) {
  const label = t(`kube.${kind}`);
  return h("li", {}, h("button", {
    type: "button", class: "nav-item", id: `nav-kube-${kind}`, "aria-label": label, title: label,
    onclick: () => go("kubernetes", kind),
  }, icon(`kube-${kind}`), h("span", { class: "label", text: label }), h("span", { class: "count" })));
}

// Routes appear only for an OpenShift context. Off one, the Routes page falls back to Pods.
let kubeOpenShiftRequest = 0;
async function refreshKubeOpenShift() {
  const generation = ++kubeOpenShiftRequest;
  let openshift = false;
  try { openshift = await API.kubeOpenShift(S.kubeContext); }
  catch { /* An unreachable cluster shows the plain kinds. */ }
  if (generation !== kubeOpenShiftRequest) return;
  S.kubeOpenShift = openshift;
  if (!openshift && S.kubeKind === "routes") {
    if (S.view === "kubernetes") go("kubernetes", "pods");
    else S.kubeKind = "pods";
  }
  updateKubeNav();
  kubePanel?.showKinds();
}

let kubeCountRequest = 0;
async function refreshKubeCounts() {
  const generation = ++kubeCountRequest;
  S.kubeCounts = {};
  updateKubeNav();
  try {
    const counts = await API.kubeCounts(S.kubeContext, S.kubeAllNamespaces);
    if (generation === kubeCountRequest) { S.kubeCounts = counts; updateKubeNav(); }
  } catch { /* An unavailable count stays blank. */ }
}

function updateKubeNav() {
  const active = S.view === "kubernetes";
  const children = document.getElementById("nav-kube-children");
  const routes = document.getElementById("nav-kube-routes");
  if (S.kubeOpenShift && !routes) children.append(kubeNavItem("routes"));
  if (!S.kubeOpenShift && routes) routes.parentElement.remove();
  const kinds = kubeKinds();
  // The parent's total stays the four kinds every cluster has.
  const counts = KUBE_KINDS.map(kind => S.kubeCounts[kind]);
  const total = counts.every(Number.isInteger) ? counts.reduce((sum, n) => sum + n, 0) : null;
  for (const [id, count] of [["open", total], ...kinds.map(kind => [kind, S.kubeCounts[kind]])]) {
    const button = document.getElementById(`nav-kube-${id}`);
    const badge = button.querySelector(".count");
    badge.textContent = count ?? "";
    button.title = count == null ? button.getAttribute("aria-label") : `${button.getAttribute("aria-label")} (${count})`;
  }
  document.getElementById("nav-kube-toggle").setAttribute("aria-expanded", String(S.kubeExpanded));
  children.hidden = !S.kubeExpanded;
  for (const [id, current] of [
    ["nav-kube-open", active && !S.kubeExpanded],
    ...kinds.map((kind) => [`nav-kube-${kind}`, active && S.kubeExpanded && S.kubeKind === kind]),
  ]) {
    const button = document.getElementById(id);
    if (current) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
}

function setupTrainingNav() {
  const label = t("nav.tutorials");
  const open = h("button", {
    type: "button", id: "nav-training-open", class: "nav-item", "aria-label": label, title: label,
    onclick: () => go("tutorials"),
  }, icon("tutorials"), h("span", { class: "label", text: label }));
  const toggle = h("button", {
    type: "button", id: "nav-training-toggle", class: "icon-button kube-toggle",
    "aria-controls": "nav-training-children",
    onclick: () => { S.trainingExpanded = !S.trainingExpanded; updateTrainingNav(); },
  }, icon("back"));
  const children = h("ul", { id: "nav-training-children", class: "nav kube-subnav", "aria-label": label });
  document.getElementById("nav-training").replaceChildren(
    h("li", {}, h("div", { class: "kube-nav-header" }, open, toggle), children));
}

function updateTrainingNav() {
  const open = document.getElementById("nav-training-open");
  if (!open) return;
  const toggle = document.getElementById("nav-training-toggle");
  const children = document.getElementById("nav-training-children");
  const label = t("nav.tutorials");
  const toggleLabel = t("training.modules");
  open.title = label;
  open.setAttribute("aria-label", label);
  open.querySelector(".label").textContent = label;
  toggle.title = toggleLabel;
  toggle.setAttribute("aria-label", toggleLabel);
  children.setAttribute("aria-label", toggleLabel);
  const entries = trainingEntries();
  const key = JSON.stringify(entries);
  if (children.dataset.modules !== key) {
    children.dataset.modules = key;
    children.replaceChildren(...entries.map(entry => h("li", {}, h("button", {
      type: "button", class: "nav-item", "data-view": trainingView(entry.id), title: entry.title,
      "aria-label": entry.label, onclick: () => go(trainingView(entry.id)),
    }, icon("tutorials"), h("span", { class: "label", text: entry.label })))));
  }
  const expanded = entries.length > 0 && S.trainingExpanded;
  toggle.hidden = entries.length === 0;
  toggle.setAttribute("aria-expanded", String(expanded));
  children.hidden = !expanded;
  open.toggleAttribute("data-active", S.view.startsWith("training:") && !expanded);
  if (S.view === "tutorials") open.setAttribute("aria-current", "page");
  else open.removeAttribute("aria-current");
  for (const child of children.querySelectorAll("button")) {
    if (child.dataset.view === S.view) child.setAttribute("aria-current", "page");
    else child.removeAttribute("aria-current");
  }
}

function renderNav() {
  const nav = document.getElementById("nav");
  const item = (view, label, count, symbol = view) => h("li", {}, h("button", {
    type: "button", class: "nav-item", "aria-current": S.view === view ? "page" : null, onclick: () => go(view),
    "aria-label": label, title: count == null ? label : `${label} (${count})`,
  }, icon(symbol), h("span", { class: "label", text: label }), h("span", { class: "count", text: count ?? "" })));
  updateMigrationNav();
  nav.replaceChildren(...KINDS.map(k => item(k, t(`nav.${k}`), S.loaded && !S.listErrors[k] ? S.lists[k].length : null)),
    item("models", t("nav.models"), null, "vm"));
  updateTrainingNav();
  document.getElementById("nav-practice").replaceChildren(
    ...(S.practiceContainers ? [item("lab-containers", t("training.containers"), null, "containers")] : []),
    ...(S.practiceInstalled ? [item("lab-volumes", t("practice.volumes"), null, "volumes")] : []));
  updateKubeNav();
  document.getElementById("nav-extensions").replaceChildren(item("extensions", t("nav.extensions"), null, "kube-deployments"));
  if (windowBar) {
    const [reload, settings] = document.getElementById("topbar-tools").children;
    for (const [button, label] of [[reload, t("action.refresh")], [settings, t("nav.settings")]]) {
      button.title = label;
      button.setAttribute("aria-label", label);
    }
    if (S.view === "settings") settings.setAttribute("aria-current", "page");
    else settings.removeAttribute("aria-current");
    return;
  }
  document.getElementById("nav-bottom").replaceChildren(item("settings", t("nav.settings"), null));
}

function engineState() {
  const e = S.engine;
  if (!e) return ["checking", t("engine.checking")];
  if (e.engine && e.engine !== "podbag") return e.reachable ? ["ok", t("engine.running")] : ["down", t("engine.down")];
  if (e.vm === "running") return ["ok", t("engine.vmRunning")];
  if (e.vm && !e.reachable) return ["off", t("engine.vmStopped")];
  if (e.reachable) return ["ok", t("engine.running")];
  return ["down", t("engine.down")];
}

function renderEngine() {
  const el = document.getElementById("engine");
  if (!el) return;
  const [cls, text] = engineState();
  const name = (S.engine && S.engine.engineName) || "podbag";
  el.className = `engine engine-${cls}`;
  el.title = `${name}: ${text}${S.engine && (S.engine.version || S.engine.error) ? `\n${S.engine.version || S.engine.error}` : ""}`;
  el.replaceChildren(h("span", { class: "dot", "aria-hidden": "true" }), h("span", { class: "label", text: `${name} · ${text}` }));
  // The VM card shows the state instead, for podbag with a VM.
  el.hidden = !!S.vm && hasVm();
}

// The engine preference shared by Desktop and the TUI.
function engineChoice() {
  const name = S.connection.name;
  const option = (value, text) => h("option", { value, text });
  const select = h("select", { class: "engine-picker", id: "settings-engine-picker",
    "aria-label": t("settings.engine") },
    option("podbag", "podbag"), option("docker", "Docker"), option("podman", "Podman"), option("kiri", "Kiri"));
  if (S.contexts.length) select.append(h("optgroup", { label: t("engine.contexts") },
    S.contexts.map((context) => option("context:" + context, context))));
  if (![...select.options].some((o) => o.value === name)) select.append(option(name, name));
  if (S.engine?.platform !== "windows" && !navigator.userAgent.includes("Windows")) select.append(option("custom", t("engine.socket")));
  select.value = name;
  select.onchange = () => {
    const next = select.value;
    select.value = name;
    if (next === "custom") {
      askDialog({ title: t("engine.socket"), label: t("settings.socket"), placeholder: "unix:///path/to/docker.sock",
        okLabel: t("engine.connect"), work: switchEngine });
    } else {
      switchEngine(next).catch((err) => toast(String(err), true));
    }
  };
  return select;
}

// ---- the sidebar's width, collapse and divider ----

const narrowWindow = window.matchMedia("(max-width: 720px)");
const clampWidth = (w) => Math.min(SIDEBAR.max, Math.max(SIDEBAR.min, Math.round(Number(w) || SIDEBAR.width)));
let sidebarDrag = null;
let sidebarExpandedInNarrowWindow = false;

function sidebarWidth() {
  if (sidebarDrag) return sidebarDrag.width;
  return S.settings.sidebarCollapsed || (narrowWindow.matches && !sidebarExpandedInNarrowWindow)
    ? SIDEBAR.rail : clampWidth(S.settings.sidebarWidth);
}

function applySidebar() {
  const app = document.getElementById("app");
  const width = sidebarWidth();
  const collapsed = width === SIDEBAR.rail;
  app.style.setProperty("--sidebar-w", `${width}px`);
  app.classList.toggle("collapsed", collapsed);
  const splitter = document.getElementById("splitter");
  splitter.setAttribute("aria-valuenow", String(width));
  splitter.setAttribute("aria-label", t("sidebar.resize"));
  const toggle = document.getElementById("sidebar-toggle");
  const label = collapsed ? t("sidebar.show") : t("sidebar.hide");
  if (!toggle.firstChild) toggle.append(icon("sidebar"));
  toggle.setAttribute("aria-expanded", collapsed ? "false" : "true");
  toggle.setAttribute("aria-label", label);
  toggle.title = `${label} (${IS_MAC ? "⌃⌘S" : "Ctrl+B"})`;
}

let saveSidebarTimer = null;
function setSidebarWidth(width) {
  const collapsed = width === SIDEBAR.rail;
  S.settings.sidebarCollapsed = collapsed;
  if (!collapsed) S.settings.sidebarWidth = clampWidth(width);
  sidebarExpandedInNarrowWindow = !collapsed && (narrowWindow.matches || sidebarExpandedInNarrowWindow);
  applySidebar();
  clearTimeout(saveSidebarTimer);
  saveSidebarTimer = setTimeout(() => API.setSettings(S.settings).catch(() => {}), 300);
}

function toggleSidebar() {
  if (sidebarDrag) return;
  setSidebarWidth(sidebarWidth() === SIDEBAR.rail ? clampWidth(S.settings.sidebarWidth) : SIDEBAR.rail);
}

function setupSidebar() {
  const splitter = document.getElementById("splitter");
  const app = document.getElementById("app");
  splitter.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || sidebarDrag) return;
    e.preventDefault();
    splitter.setPointerCapture(e.pointerId);
    const width = sidebarWidth();
    sidebarDrag = { id: e.pointerId, x: e.clientX, startWidth: width, width };
    app.classList.add("resizing");
  });
  splitter.addEventListener("pointermove", (e) => {
    if (sidebarDrag?.id !== e.pointerId) return;
    const dir = document.documentElement.dir === "rtl" ? -1 : 1;
    const width = sidebarDrag.startWidth + (e.clientX - sidebarDrag.x) * dir;
    const threshold = sidebarDrag.startWidth === SIDEBAR.rail ? SIDEBAR.expandAt : SIDEBAR.collapseAt;
    sidebarDrag.width = width <= threshold ? SIDEBAR.rail : clampWidth(width);
    applySidebar();
  });
  const end = (e) => {
    if (sidebarDrag?.id !== e.pointerId) return;
    const { startWidth, width } = sidebarDrag;
    sidebarDrag = null;
    app.classList.remove("resizing");
    if (e.type === "pointerup" && width !== startWidth) setSidebarWidth(width);
    else applySidebar();
  };
  splitter.addEventListener("pointerup", end);
  splitter.addEventListener("pointercancel", end);
  splitter.addEventListener("lostpointercapture", end);
  splitter.addEventListener("dblclick", () => setSidebarWidth(SIDEBAR.width));
  splitter.addEventListener("keydown", (e) => {
    if (sidebarDrag) return;
    const step = e.shiftKey ? 40 : 10;
    const width = sidebarWidth();
    const smaller = width <= SIDEBAR.min ? SIDEBAR.rail : Math.max(SIDEBAR.min, width - step);
    const larger = Math.max(SIDEBAR.min, width + step);
    const rtl = document.documentElement.dir === "rtl";
    const keys = {
      ArrowLeft: rtl ? larger : smaller, ArrowRight: rtl ? smaller : larger,
      ArrowUp: larger, ArrowDown: smaller, Home: SIDEBAR.rail, End: SIDEBAR.max,
    };
    if (e.key === "Enter") {
      e.preventDefault();
      toggleSidebar();
    } else if (Object.hasOwn(keys, e.key)) {
      e.preventDefault();
      setSidebarWidth(keys[e.key]);
    }
  });
  document.getElementById("sidebar-toggle").addEventListener("click", toggleSidebar);
  document.getElementById("nav-back").addEventListener("click", () => travel(-1));
  document.getElementById("nav-forward").addEventListener("click", () => travel(1));
  // Follow the window width until the user expands the sidebar in a narrow window.
  narrowWindow.addEventListener("change", applySidebar);
  applySidebar();
  renderHistory();
}

// ---- the VM card ----

// Memory in MiB as text: 512 MB, 1.5 GB.
function mib(m) {
  if (m == null) return "—";
  const fmt = (n, unit) => new Intl.NumberFormat(I18N.lang, { style: "unit", unit, unitDisplay: "short", maximumFractionDigits: 1 }).format(n);
  return m >= 1024 ? fmt(m / 1024, "gigabyte") : fmt(m, "megabyte");
}

function percent(p) {
  return new Intl.NumberFormat(I18N.lang, { style: "percent", maximumFractionDigits: 1 }).format((p || 0) / 100);
}

// 2 h 5 min, 3 min, 2 days.
function duration(secs) {
  if (secs == null) return "—";
  const fmt = (n, unit) => new Intl.NumberFormat(I18N.lang, { style: "unit", unit, unitDisplay: "short", maximumFractionDigits: 0 }).format(n);
  if (secs >= 86400) return fmt(Math.floor(secs / 86400), "day");
  if (secs >= 3600) return `${fmt(Math.floor(secs / 3600), "hour")} ${fmt(Math.floor((secs % 3600) / 60), "minute")}`;
  return fmt(Math.max(1, Math.floor(secs / 60)), "minute");
}

function vmStateKey(state) {
  const known = { running: "running", standby: "standby", starting: "starting", stopped: "stopped", "not set up": "notSetUp" };
  return known[state] || "unknown";
}

function vmChip(state) {
  const key = vmStateKey(state);
  const cls = key === "running" ? "running" : key === "starting" ? "restarting" : "created";
  return h("span", { class: `chip chip-${cls}` }, h("span", { class: "dot", "aria-hidden": "true" }), t(`vm.state.${key}`));
}

let vmBusy = false;
async function vmAction(action) {
  const n = (S.vm && S.vm.containers) || 0;
  if ((action === "stop" || action === "restart") && n > 0) {
    if (!(await confirmDialog(t(action === "stop" ? "confirm.stopVm" : "confirm.restartVm", { n }), t(action === "stop" ? "action.stopVm" : "action.restartVm")))) return;
  }
  vmBusy = true;
  renderVmCard();
  if (S.view === "settings") renderVmSettings();
  try {
    await API.vm(action);
  } catch (e) {
    toast(String(e), true);
  }
  vmBusy = false;
  await refreshVm();
}

function renderVmCard() {
  const card = document.getElementById("vm-card");
  const vm = S.vm;
  renderEngine();
  if (!vm || !hasVm()) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  const running = vm.state === "running";
  const stoppable = running || vm.state === "standby" || vm.state === "starting";
  const rows = [];
  if (vm.containers != null || running) rows.push([t("vm.containers"), String(vm.containers ?? 0)]);
  if (vm.cpuPercent != null || vm.inUse) {
    const cpus = vm.inUse ? vm.inUse.cpus : null;
    rows.push([t("vm.cpu"), [vm.cpuPercent != null ? percent(vm.cpuPercent) : null, cpus].filter((x) => x != null).join(" · ")]);
  }
  let bar = null;
  if (vm.memoryTotalMiB) {
    rows.push([t("vm.memory"), `${mib(vm.memoryUsedMiB || 0)} / ${mib(vm.memoryTotalMiB)}`]);
    const used = Math.min(100, Math.round(((vm.memoryUsedMiB || 0) / vm.memoryTotalMiB) * 100));
    bar = h("div", { class: "meter", role: "meter", "aria-label": t("vm.memory"), "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": String(used) },
      h("span", { style: `width: ${used}%` }));
  }
  const stateText = t(`vm.state.${vmStateKey(vm.state)}`);
  const btn = stoppable
    ? button(null, t("action.stopVm"), () => vmAction("stop"), "vm-button")
    : button(null, t("action.startVm"), () => vmAction("start"), "vm-button primary");
  btn.disabled = vmBusy || vm.state === "not set up";
  card.setAttribute("aria-label", t("vm.title"));
  put(card,
    h("div", { class: "vm-head", title: `${t("vm.title")}: ${stateText}` },
      h("span", { class: `dot ${running ? "dot-on" : ""}`, "aria-hidden": "true" }),
      h("span", { class: "vm-name", text: t("vm.title") }),
      h("span", { class: "vm-state", text: stateText })),
    rows.length ? h("dl", { class: "vm-rows" }, rows.map(([k, v]) => [h("dt", { text: k }), h("dd", { text: v })])) : null,
    bar,
    btn,
    // The rail shows only this: the state as a dot, its text in the tooltip.
    h("button", { type: "button", class: "icon-button vm-rail", title: `${t("vm.title")}: ${stateText}`, "aria-label": `${t("vm.title")}: ${stateText}`, onclick: () => openSettingsSection("vm-settings") },
      icon("vm"), h("span", { class: `dot ${running ? "dot-on" : ""}`, "aria-hidden": "true" })));
}

// ---- the status bar ----

function statusValue(full, compact) {
  return [h("span", { class: "status-full truncate", text: full }), h("span", { class: "status-compact", "aria-hidden": "true" }, compact)];
}

function renderStatusbar() {
  const bar = document.getElementById("statusbar");
  if (!bar) return;
  const u = S.updates;
  const left = [];
  if (u && u.engine) {
    left.push(h("button", { type: "button", class: "status-item", title: t("settings.updates"), onclick: openUpdates }, `podbag ${u.engine}`));
  }
  if (u && u.newer && u.latest) {
    const label = t("status.available", { version: u.latest });
    left.push(h("button", { type: "button", class: "status-item status-update", onclick: openUpdates, title: label, "aria-label": label },
      icon("update"), h("span", { class: "status-action-label", text: label })));
  }
  left.push(h("button", { type: "button", class: "status-item", title: t("status.terminal"), "aria-label": t("status.terminal"), onclick: openPlainTerminal },
    icon("shell"), h("span", { class: "status-action-label", text: t("status.terminal") })));
  const right = [];
  const [cls, text] = S.vm && hasVm() ? [S.vm.state === "running" ? "ok" : "off", `${t("vm.title")}: ${t(`vm.state.${vmStateKey(S.vm.state)}`)}`] : engineState();
  const name = (S.engine && S.engine.engineName) || "podbag";
  const engine = S.vm && hasVm() ? text : `${name}: ${text}`;
  right.push(h("span", { class: `status-item engine-${cls}`, title: engine }, h("span", { class: "dot", "aria-hidden": "true" }),
    statusValue(engine, S.vm && hasVm() ? icon("vm") : name)));
  if (S.loaded) {
    const total = S.lists.containers.length;
    const running = S.lists.containers.filter((c) => c.State === "running").length;
    const label = t("status.running", { n: running, total });
    right.push(h("span", { class: "status-item", title: label }, statusValue(label, [icon("containers"), `${running}/${total}`])));
  }
  if (S.vm && S.vm.memoryTotalMiB) {
    const used = mib(S.vm.memoryUsedMiB || 0);
    const memory = `${used} / ${mib(S.vm.memoryTotalMiB)}`;
    right.push(h("span", { class: "status-item", title: `${t("vm.memory")} · ${memory}` }, statusValue(memory, used)));
  }
  const docker = dockerCommandItem();
  if (docker) right.push(docker);
  bar.replaceChildren(h("div", { class: "status-side" }, left), h("div", { class: "status-side status-right" }, right));
}

async function openPlainTerminal() {
  try {
    await API.openTerminal("plain");
  } catch {
    toast(t("terminal.none"), true);
  }
}

function openUpdates() {
  return openSettingsSection("updates");
}

function dockerCommandItem() { return null; }

// ---- the main area ----

let shownView = null;
let kubePanel;
let migrationPanel;

function renderMain(fresh) {
  const main = document.getElementById("main");
  if (S.view === "kubernetes") {
    if (fresh || shownView !== S.view) kubePanel = renderKubernetes(main);
    shownView = S.view;
    return;
  }
  kubePanel = null;
  if (S.view === "migration") {
    if (fresh || shownView !== S.view) migrationPanel = renderMigration(main);
    shownView = S.view;
    return;
  }
  if (S.view === "extensions") {
    const redraw = fresh || shownView !== S.view;
    shownView = S.view;
    if (redraw) openExtensions(main);
    else refreshExtensionSummary(main);
    return;
  }
  if (S.view === "lab-containers") {
    if (fresh || shownView !== S.view) renderPracticeContainers(main);
    shownView = S.view;
    return;
  }
  if (S.view === "lab-volumes") {
    if (fresh || shownView !== S.view) renderPracticeVolumes(main);
    shownView = S.view;
    return;
  }
  if (S.view === "tutorials" || S.view.startsWith("training:")) {
    const rendered = fresh || shownView !== S.view ? renderTutorials(main) : undefined;
    shownView = S.view;
    return rendered;
  }
  if (S.view === "models") {
    const redraw = fresh || shownView !== S.view;
    shownView = S.view;
    if (redraw) openModelRunner(main);
    return;
  }
  if (S.view === "settings") {
    const redraw = fresh || shownView !== "settings";
    shownView = "settings";
    if (redraw) return renderSettings(main);
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

function createVolume() {
  askDialog({ title: t("volume.create"), label: t("col.name"), placeholder: "data", okLabel: t("volume.create"),
    work: async (name) => { await API.createVolume(name); refreshSoon(); } });
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
      kind === "images" && isPodbag() && S.docker && S.docker.installed ? button(registryIcon("dockerhub"), t("import.button"), importFromDocker) : null,
      kind === "images" ? button("pull", t("action.pull"), pull, "primary") : null,
      kind === "containers" ? button("start", t("run.button"), () => openRunDialog(), "primary") : null,
      kind === "volumes" ? button(null, t("volume.create"), createVolume, "primary") : null,
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
        sizeCell(o),
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
  if (kind === "images") out.push(iconButton("start", t("action.run"), () => openRunDialog(imageName(o)), "accent"));
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
  if (S.listErrors[kind]) {
    content.replaceChildren(h("p", { class: "error-text", text: S.listErrors[kind] }),
      button("refresh", t("action.retry"), refresh));
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
  content.replaceChildren(resourceTable(kind, rows, o => openDetail(kind, objectId(kind, o), "info"), o => rowActions(kind, o)));
}

function resourceTable(kind, rows, open, actions) {
  const cols = COLUMNS[kind];
  const head = h("thead", {}, h("tr", {}, cols.map(([key, c]) => h("th", { class: `col-${c}`, scope: "col", text: t(key) })),
    h("th", { class: "col-actions", scope: "col" }, h("span", { class: "visually-hidden", text: t("col.actions") }))));
  const body = h("tbody", {}, rows.map((o) => {
    const tr = h("tr", {}, cells(kind, o).map((cell, i) => h("td", { class: `col-${cols[i][1]}` }, cell)), h("td", { class: "col-actions" }, actions(o)));
    // The name opens the details; the row is clickable too, for the mouse.
    const first = tr.querySelector("td .primary-text");
    if (first) {
      const link = h("button", { type: "button", class: "link", onclick: () => open(o) });
      first.replaceWith(link);
      link.append(first);
    }
    tr.addEventListener("click", (e) => {
      if (e.target.closest("button")) return;
      open(o);
    });
    return tr;
  }));
  // A compressed image's Size cell has a second line, which needs a wider column.
  const wide = kind === "images" && rows.some(compressionOf);
  return h("table", { class: `list list-${kind}${wide ? " has-compressed" : ""}` }, head, body);
}

function emptyState(kind) {
  const cmd = isPodbag() ? "podbag" : "docker";
  const commands = {
    containers: `${cmd} run -d --name web -p 8080:80 nginx:alpine`,
    volumes: `${cmd} volume create data`,
    networks: `${cmd} network create app`,
  };
  const run = kind === "containers";
  return h("div", { class: "empty" },
    h("div", { class: "empty-icon" }, icon(kind)),
    h("h2", { text: t(`empty.${kind}.title`) }),
    run ? [h("p", { text: t("empty.containers.run") }), button("start", t("run.button"), () => openRunDialog(), "primary empty-run")] : null,
    isPodbag() || S.engine?.engine === "docker" || kind === "images" ? h("p", { text: t(`empty.${kind}.text`) }) : null,
    (isPodbag() || S.engine?.engine === "docker") && commands[kind] ? codeLine(commands[kind]) : null,
    kind === "images" ? button("pull", t("action.pull"), pull, "primary") : null);
}

function offline() {
  const e = S.engine || {};
  if (e.engine === "docker") {
    // Docker is never started from here.
    return h("div", { class: "empty" },
      h("div", { class: "empty-icon" }, icon("engine")),
      h("h2", { text: t("offline.dockerTitle") }),
      e.platform === "linux"
        ? [h("p", { text: t("offline.dockerLinux") }), codeLine("sudo systemctl start docker")]
        : h("p", { text: t("offline.dockerText") }),
      h("div", { class: "buttons-row" },
        button("refresh", t("action.retry"), () => connect(false), "primary"),
        button(null, t("switch.to", { name: "podbag" }), () => switchEngine("podbag"))));
  }
  if (!S.connection.local) {
    return h("div", { class: "empty" }, h("h2", { text: t("engine.down") }),
      h("p", { class: "error-text mono", text: e.error || e.endpoint }),
      button("refresh", t("action.retry"), () => connect(false), "primary"));
  }
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
  remember();
}

// quiet: another page comes next (it is the one remembered).
function closeDetail(quiet) {
  if (S.detail && S.detail.tab === "logs") API.logsStop();
  S.detail = null;
  if (quiet) return;
  renderMain(true);
  remember();
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
    iconButton("back", t("action.back"), () => closeDetail()),
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
    const c = compressionOf(o);
    rows = [
      field(t("col.id"), o.Id, "mono"),
      field(t("detail.tags"), (o.RepoTags || []).join(", ")),
      field(t("col.size"), size(o.Size)),
      c ? [h("dt", { text: t("detail.compression") }), h("dd", {},
        h("div", { text: t("compression.stored", { size: size(c.stored), before: size(c.original) }) }),
        h("div", { class: "secondary-text", text: t("compression.smaller", { percent: percent(c.saved) }) }))] : null,
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

async function openSettingsSection(id) {
  if (S.view !== "settings" || !document.getElementById(id)) await go("settings");
  const section = document.getElementById(id);
  if (!section || section.hidden) return;
  const search = document.getElementById("settings-search");
  if (search?.value) {
    search.value = "";
    search.dispatchEvent(new Event("input"));
  }
  section.open = true;
  section.querySelector("summary").focus({ preventScroll: true });
  section.scrollIntoView({ block: "start" });
}

function settingTitle(key, symbol) {
  return h("span", { class: "setting-title" },
    h("span", { class: `setting-icon setting-${key}` }, icon(symbol)), t(`settings.${key}`));
}

function settingDetails(id, key, symbol, value, ...body) {
  return h("details", { id },
    h("summary", searchableSetting(`settings.${key}`, `settings.search.terms.${key}`), settingTitle(key, symbol), h("span", { class: "setting-value", text: value }), icon("forward")),
    h("div", { class: "setting-body" }, body));
}

// Settings can be reopened while the operating system is updating its login entry.
let startupWrite = Promise.resolve();

function startupSettings() {
  const toggle = h("input", { type: "checkbox", id: "launch-at-login", disabled: true });
  const status = h("div", { role: "status" });
  async function readRegistration() {
    toggle.disabled = true;
    toggle.indeterminate = true;
    status.replaceChildren();
    await startupWrite;
    try {
      toggle.checked = await API.launchAtLogin();
      toggle.indeterminate = false;
      toggle.disabled = false;
    } catch (err) {
      status.replaceChildren(h("p", { class: "error-text", text: String(err) }),
        button(null, t("action.retry"), readRegistration));
    }
  }
  toggle.addEventListener("change", () => {
    startupWrite = API.setLaunchAtLogin(toggle.checked).catch((err) => toast(String(err), true));
    readRegistration();
  });
  readRegistration();
  return h("div", { class: "preference", ...searchableSetting("settings.launchAtLogin", "settings.startup settings.search.terms.startup") },
    settingTitle("startup", "power"),
    h("label", { class: "check" }, toggle, t("settings.launchAtLogin")), status);
}

async function renderSettings(main) {
  if (!S.languageNames) S.languageNames = await I18N.names();
  if (S.view !== "settings") return;
  const lang = h("select", {
    id: "language",
    onchange: async (e) => {
      S.settings.language = e.target.value;
      await saveSettings();
      await I18N.load(S.settings.language);
      setupKubeNav();
      renderAll();
      if (S.view === "settings") document.getElementById("language")?.focus();
    },
  }, h("option", { value: "system", text: t("settings.system") }),
  I18N.LANGUAGES.map((l) => h("option", { value: l, lang: l, text: S.languageNames[l] })));
  lang.value = S.settings.language;
  const themes = [["system", "display"], ["light", "sun"], ["dark", "moon"]].map(([value, symbol]) =>
    h("label", { class: "segment" },
      h("input", { type: "radio", class: "visually-hidden", name: "theme", value, checked: S.settings.theme === value,
        onchange: () => { S.settings.theme = value; applyTheme(); saveSettings(); } }),
      h("span", {}, icon(symbol), t(`settings.${value}`))));
  const e = S.engine || {};
  const openSections = Array.from(main.querySelectorAll(".settings details[open]"), (el) => el.id);
  const oldSearch = main.querySelector("#settings-search");
  const searchFocused = document.activeElement === oldSearch;
  const sections = h("div", { class: "settings-sections" },
    h("div", { class: "card" },
      startupSettings(),
      h("div", { class: "preference", ...searchableSetting("settings.language", "settings.search.terms.language") },
        h("label", { for: "language" }, settingTitle("language", "globe")), lang),
      h("div", { class: "preference", ...searchableSetting("settings.theme", "settings.light settings.dark settings.search.terms.theme") },
        h("span", { id: "theme-label" }, settingTitle("theme", "appearance")),
        h("div", { class: "segments themes", role: "radiogroup", "aria-labelledby": "theme-label" }, themes))),
    h("div", { class: "card" },
      settingDetails("engine-settings", "engine", "engine", e.engineName,
        h("div", { class: "setting-row" }, h("span", { class: "secondary-text", text: t("settings.engine") }), engineChoice()),
        h("p", { class: "hint", text: t("settings.engineNote") }),
        h("dl", { class: "fields" },
          field(t("settings.socket"), e.endpoint, "mono"), field(t("settings.version"), e.version)),
        dockerSettings(),
        compressionSetting()),
      settingDetails("vm-settings", "vm", "vm"),
      settingDetails("updates", "updates", "update"),
      settingDetails("terminal-command", "terminalCommand", "shell"),
      settingDetails("about-settings", "about", "inspect", null, h("p", { text: t("settings.aboutText") }))));
  put(main,
    h("header", { class: "toolbar" }, h("h1", { text: t("settings.title") })),
    h("div", { class: "content settings" }, settingsSearch(sections, oldSearch?.value), sections));
  if (searchFocused) main.querySelector("#settings-search").focus();
  renderVmSettings(true);
  renderUpdates();
  renderTerminalCommand();
  readCompressionSwitch();
  for (const id of openSections) document.getElementById(id).open = true;
}

// The parts of the docker command's settings (filled in when known).
function dockerSettings() {
  return null;
}

// ---- settings: the virtual machine ----

let vmFormDirty = false;

// The VM's section. `fresh`: build the form again (otherwise only the state changes,
// so what the user types stays).
function renderVmSettings(fresh) {
  const section = document.getElementById("vm-settings");
  if (!section) return;
  const box = section.querySelector(".setting-body");
  const vm = S.vm;
  section.hidden = !vm || !hasVm();
  if (section.hidden) {
    box.replaceChildren();
    return;
  }
  section.querySelector(".setting-value").textContent = t(`vm.state.${vmStateKey(vm.state)}`);
  const statusPart = vmStatusPart(vm);
  const old = box.querySelector(".vm-status");
  if (old && !fresh && box.querySelector(".vm-form, .vm-note")) {
    old.replaceWith(statusPart);
    const pending = box.querySelector(".vm-pending");
    if (pending) pending.hidden = !vm.pending;
    if (!vmFormDirty && vm.resourcesEditable) fillVmForm(box.querySelector(".vm-form"), vm);
    return;
  }
  vmFormDirty = false;
  put(box,
    statusPart,
    vm.resourcesEditable ? vmForm(vm) : h("p", { class: "hint vm-note", text: t("settings.vmWsl") }));
}

function vmStatusPart(vm) {
  const running = vm.state === "running";
  const stoppable = running || vm.state === "standby" || vm.state === "starting";
  const rows = [h("dt", { text: t("settings.vmState") }), h("dd", {}, vmChip(vm.state))];
  if (vm.inUse) {
    rows.push(...field(t("settings.vmInUse"), [
      t("settings.cpusValue", { n: vm.inUse.cpus }),
      mib(vm.inUse.memoryMiB),
      idleText(vm.inUse.idleSeconds),
    ].join(" · ")));
  }
  if (vm.uptimeSeconds != null) rows.push(...field(t("settings.vmUptime"), duration(vm.uptimeSeconds)));
  if (vm.disk) {
    rows.push(...field(t("settings.vmDisk"), t("settings.vmDiskValue", { used: size(vm.disk.allocatedBytes), size: size(vm.disk.sizeBytes) })));
  }
  const buttons = [
    button("start", t("action.startVm"), () => vmAction("start"), stoppable ? "" : "primary"),
    button("stop", t("action.stopVm"), () => vmAction("stop")),
    button("restart", t("action.restartVm"), () => vmAction("restart")),
  ];
  buttons[0].disabled = vmBusy || stoppable || vm.state === "not set up";
  buttons[1].disabled = vmBusy || !stoppable;
  buttons[2].disabled = vmBusy || !running;
  return h("div", { class: "vm-status" },
    h("dl", { class: "fields" }, rows),
    vm.error ? h("p", { class: "error-text", text: vm.error }) : null,
    h("div", { class: "buttons-row" }, buttons));
}

function idleText(secs) {
  if (!secs) return t("settings.idleNever");
  return t("settings.idleAfter", { time: duration(secs) });
}

const ENV_FIELD = { PODBAG_VM_CPUS: "cpus", PODBAG_VM_MEMORY: "memory", PODBAG_VM_IDLE: "idle" };

function vmSharedFolders() {
  const input = h("input", { id: "vm-share-path", type: "text", placeholder: "/Volumes/Projects" });
  const list = h("ul", { class: "shared-folders", "aria-label": t("settings.sharedFolders") });
  const add = () => {
    const path = input.value;
    if (!path.startsWith("/")) {
      input.setCustomValidity(t("settings.sharedFolderAbsolute"));
      input.reportValidity();
      return;
    }
    if (!vmSharedPaths(list).includes(path)) list.append(vmSharedFolderRow(path));
    vmFormDirty = true;
    input.value = "";
    input.focus();
  };
  input.oninput = () => input.setCustomValidity("");
  input.onkeydown = (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    add();
  };
  return h("div", { class: "vm-shares form-field", "data-setting-search": "settings.sharedFolders" },
    h("label", { for: input.id, text: t("settings.sharedFolders") }),
    h("div", { class: "shared-folder" }, input, button(null, t("settings.addSharedFolder"), add)),
    list,
    h("p", { class: "hint", text: t("settings.sharedFoldersHint") }));
}

function vmSharedFolderRow(path) {
  const row = h("li", { class: "shared-folder", "data-shared-path": path },
    h("span", { class: "mono", text: path }),
    button(null, t("settings.removeSharedFolder"), () => {
      const next = row.nextElementSibling || row.previousElementSibling;
      row.remove();
      (next?.querySelector("button") || document.getElementById("vm-share-path")).focus();
      vmFormDirty = true;
    }));
  row.querySelector("button").setAttribute("aria-label", t("settings.removeSharedFolderName", { path }));
  return row;
}

function vmSharedPaths(list) {
  return Array.from(list.children, (row) => row.dataset.sharedPath);
}

function samePaths(a, b) {
  return a.length === b.length && a.every((path, index) => path === b[index]);
}

function vmForm(vm) {
  const l = vm.limits || {};
  const over = new Map((vm.overriddenBy || []).map((name) => [ENV_FIELD[name], name]));
  const input = (id, label, props, hint) => {
    const key = id.slice(3);
    const value = h("input", { id, type: "number", inputmode: "decimal", required: true, ...props });
    const slider = id === "vm-idle" ? null : h("input", { type: "range", "aria-label": label, ...props });
    value.oninput = () => { vmFormDirty = true; if (slider) slider.value = value.value; };
    if (slider) slider.oninput = () => { vmFormDirty = true; value.value = slider.value; };
    const variable = over.get(key);
    return h("div", { class: "form-field", ...searchableSetting(`settings.${key}`, `settings.search.terms.${key}`) },
      h("label", { for: id, text: label }), value, slider,
      h("span", { class: "hint", text: hint }),
      variable ? h("span", { class: "hint warn", text: t("settings.vmOverridden", { name: variable }) }) : null);
  };
  const form = h("form", { class: "vm-form", novalidate: true, onsubmit: (e) => { e.preventDefault(); applyVm(form); } },
    h("div", { class: "form-grid" },
      input("vm-cpus", t("settings.cpus"), { min: l.cpusMin || 1, max: l.cpusMax || 64, step: 1 },
        t("settings.limits", { min: l.cpusMin || 1, max: l.cpusMax || "?" })),
      input("vm-memory", t("settings.memory"), { min: (l.memoryMinMiB || 1024) / 1024, max: (l.memoryMaxMiB || 65536) / 1024, step: 0.25 },
        t("settings.limits", { min: mib(l.memoryMinMiB || 1024), max: mib(l.memoryMaxMiB) })),
      input("vm-idle", t("settings.idle"), { min: 0, max: 1440, step: 1 }, t("settings.idleHint"))),
    vm.sharedPaths ? vmSharedFolders() : null,
    h("p", { class: "hint vm-pending", text: t("settings.vmPending"), hidden: !vm.pending }),
    h("p", { class: "error-text vm-error", "aria-live": "polite" }),
    h("div", { class: "buttons-row" }, h("button", { type: "submit", class: "button primary", text: t("settings.apply") })));
  fillVmForm(form, vm);
  return form;
}

function fillVmForm(form, vm) {
  if (!form) return;
  const next = vm.next || vm.inUse || {};
  form.querySelector("#vm-cpus").value = next.cpus ?? "";
  form.querySelector("#vm-memory").value = next.memoryMiB != null ? String(next.memoryMiB / 1024) : "";
  for (const range of form.querySelectorAll('input[type="range"]')) range.value = range.previousElementSibling.value;
  form.querySelector("#vm-idle").value = next.idleSeconds != null ? String(Math.round(next.idleSeconds / 60)) : "";
  const list = form.querySelector(".shared-folders");
  const paths = vm.sharedPaths?.next || [];
  if (list && !samePaths(paths, vmSharedPaths(list))) list.replaceChildren(...paths.map(vmSharedFolderRow));
}

async function applyVm(form) {
  const vm = S.vm || {};
  const next = vm.next || {};
  const error = form.querySelector(".vm-error");
  error.textContent = "";
  for (const el of form.querySelectorAll("input")) {
    if (!el.checkValidity()) {
      error.textContent = `${form.querySelector(`label[for="${el.id}"]`).textContent}: ${el.validationMessage}`;
      el.focus();
      return;
    }
  }
  const cpus = Number(form.querySelector("#vm-cpus").value);
  const memoryMiB = Math.round(Number(form.querySelector("#vm-memory").value) * 1024);
  const idleSeconds = Math.round(Number(form.querySelector("#vm-idle").value) * 60);
  const change = {};
  if (cpus !== next.cpus) change.cpus = cpus;
  if (memoryMiB !== next.memoryMiB) change.memoryMiB = memoryMiB;
  if (idleSeconds !== next.idleSeconds) change.idleSeconds = idleSeconds;
  const list = form.querySelector(".shared-folders");
  const paths = list ? vmSharedPaths(list).sort() : [];
  if (list && !samePaths(paths, (vm.sharedPaths?.next || []).slice().sort())) change.sharedPaths = paths;
  if (!Object.keys(change).length && !vm.pending) {
    toast(t("settings.vmSame"));
    return;
  }
  if (!Object.keys(change).length) change.cpus = cpus; // only to restart with them
  let restart = false;
  const inUse = vm.inUse || {};
  const differs = cpus !== inUse.cpus || memoryMiB !== inUse.memoryMiB || idleSeconds !== inUse.idleSeconds
    || (list && !samePaths(paths, (vm.sharedPaths?.inUse || []).slice().sort()));
  if (vm.state === "running" && differs) {
    const n = vm.containers || 0;
    const answer = await chooseDialog({
      title: t("settings.vmApplyTitle"),
      text: n > 0 ? `${t("settings.vmApplyText")}\n\n${t("settings.vmApplyWarn", { n })}` : t("settings.vmApplyText"),
      buttons: [
        { value: "", label: t("action.cancel") },
        { value: "later", label: t("settings.nextStart") },
        { value: "now", label: t("settings.restartNow"), cls: n > 0 ? "danger" : "primary" },
      ],
    });
    if (!answer) return;
    restart = answer === "now";
  }
  const submit = form.querySelector("button[type=submit]");
  submit.disabled = true;
  try {
    await API.vmSet({ ...change, restart });
    vmFormDirty = false;
    toast(restart ? t("settings.vmRestarted") : t("settings.vmSaved"));
  } catch (e) {
    error.textContent = String(e);
  }
  submit.disabled = false;
  await refreshVm();
}

// A dialog with several answers; resolves with the chosen value ("" for none).
// `symbol`: an icon next to the title.
function chooseDialog({ title, text, buttons, symbol }) {
  const heading = h("h2", { id: "choose-title", text: title });
  const d = h("dialog", { class: "choose", "aria-labelledby": "choose-title" },
    h("form", { method: "dialog" },
      symbol ? h("div", { class: "choose-head" }, h("span", { class: "choose-icon" }, icon(symbol)), heading) : heading,
      h("p", { text }),
      h("div", { class: "buttons" }, buttons.map((b, i) => h("button", {
        value: b.value, class: `button ${b.cls || ""}`, autofocus: i === 0,
      }, b.icon ? icon(b.icon) : null, b.label)))));
  document.body.append(d);
  d.showModal();
  return new Promise((resolve) => {
    d.addEventListener("close", () => {
      resolve(d.returnValue || "");
      d.remove();
    }, { once: true });
  });
}

async function showFirstLaunchStarPrompt() {
  if (!(await API.takeStarPrompt().catch(() => false))) return;
  const answer = await chooseDialog({
    title: t("star.title"),
    text: t("star.text"),
    buttons: [
      { value: "", label: t("star.skip") },
      { value: "open", label: t("star.open"), cls: "primary star-prompt-action", icon: "star" },
    ],
  });
  if (answer === "open") {
    try {
      await API.openRepository();
    } catch {
      toast(t("star.openFailed"), true);
    }
  }
}

// ---- image compression ----

// How the setting can be read: "read" (it can), "stopped" (podbag's VM is off, or its
// state is unknown: reading in the VM would start it), or null (another engine). Only
// macOS and Windows have a VM; elsewhere vmStatus gives null.
async function compressionAccess() {
  if (!S.connection || !S.connection.local) return null;
  const vm = await API.vmStatus().catch(() => ({ state: "unknown" }));
  return !vm || vm.state === "running" ? "read" : "stopped";
}

function compressionAnswered() {
  if (S.settings.compressionPromptSeen) return;
  S.settings.compressionPromptSeen = true;
  saveSettings();
}

// How long an image stays unused before it is compressed: whole days, else hours, else
// minutes. Intl gives the plural of each language.
function compressionAge(secs) {
  const fmt = (n, unit) => new Intl.NumberFormat(I18N.lang, { style: "unit", unit, unitDisplay: "long", maximumFractionDigits: 0 }).format(n);
  if (secs % 86400 === 0) return fmt(secs / 86400, "day");
  if (secs % 3600 === 0) return fmt(secs / 3600, "hour");
  return fmt(Math.max(1, Math.round(secs / 60)), "minute");
}

// The switch's hint: the engine's own age when its state is known (0: at once), else the
// default 7 days.
function compressionHint(state) {
  const after = state?.after;
  if (after === 0) return t("compression.hintNow");
  return Number.isInteger(after) && after > 0
    ? t("compression.hintAfter", { time: compressionAge(after) })
    : t("compression.hint");
}

// Asked once, on a new install and after an update to this version, for podbag only.
// When the setting can be read, only while compression is off. A stopped VM is not
// started to read it: the question comes anyway, and Turn on starts the VM.
async function showCompressionPrompt() {
  if (S.settings.compressionPromptSeen) return;
  const access = await compressionAccess();
  if (!access) return;
  if (access === "read") {
    let state;
    try {
      state = await API.compression();
    } catch {
      return;
    }
    if (state?.mode === "idle") compressionAnswered();
    if (state?.mode !== "off") return;
  }
  // Another dialog is open (the user is busy): ask at the next launch.
  if (document.querySelector("dialog[open]")) return;
  const answer = await chooseDialog({
    title: t("compression.title"),
    text: t("compression.text"),
    symbol: "compress",
    buttons: [
      { value: "", label: t("compression.later") },
      { value: "on", label: t("compression.turnOn"), cls: "primary" },
    ],
  });
  if (answer !== "on") {
    compressionAnswered();
    toast(t("compression.laterNote"));
    return;
  }
  let error = null;
  try {
    // An environment setting (PODBAG_IMAGE_COMPRESSION) wins over the saved mode.
    if ((await API.compression("idle"))?.mode !== "idle") error = t("compression.overridden");
  } catch (e) {
    error = String(e);
  }
  if (error) {
    toast(t("compression.failed", { error }), true);
    return;
  }
  compressionAnswered();
  toast(t("compression.done"));
  readCompressionSwitch();
}

// The switch's write while it runs (it never fails): a Settings view opened meanwhile
// reads the mode after it.
let compressionWrite = null;

// The switch in the engine's settings (podbag only).
function compressionSetting() {
  if (!S.connection || !S.connection.local) return null;
  const toggle = h("input", { type: "checkbox", role: "switch", class: "switch", id: "compression-switch",
    disabled: true, "aria-describedby": "compression-hint" });
  const hint = h("p", { class: "hint", id: "compression-hint", text: compressionHint(null) });
  // Where to see what it saves; shown only while compression is on.
  const images = h("p", { class: "hint", id: "compression-images", hidden: true, text: t("compression.imagesNote") });
  toggle.addEventListener("change", async () => {
    const on = toggle.checked;
    toggle.disabled = true;
    const write = API.compression(on ? "idle" : "off");
    const done = write.then(() => {}, () => {});
    compressionWrite = done;
    let state;
    let failed = false;
    try {
      state = await write;
      // A reply without a mode does not say what happened: it fails like an error.
      if (state?.mode !== "idle" && state?.mode !== "off") throw t("error.title");
      compressionAnswered();
      // An environment setting (PODBAG_IMAGE_COMPRESSION) wins over the saved mode.
      if (state.mode !== (on ? "idle" : "off")) {
        toast(on ? t("compression.failed", { error: t("compression.overridden") }) : t("compression.offOverridden"), true);
      }
    } catch (e) {
      failed = true;
      toast(String(e), true);
    }
    if (compressionWrite === done) compressionWrite = null;
    // Settings may have been left or opened again meanwhile: the switch on screen now
    // shows the result, and none is changed when Settings is not shown.
    const shown = document.getElementById("compression-switch");
    if (!shown) return;
    shown.checked = failed ? !on : state?.mode === "idle";
    if (!failed) document.getElementById("compression-hint").textContent = compressionHint(state);
    showImagesNote(shown);
    // A new switch comes back when its own read (readCompressionSwitch) is done.
    if (shown === toggle) shown.disabled = false;
  });
  return h("div", { class: "compression-setting", ...searchableSetting("compression.label", "settings.search.terms.compression") },
    h("div", { class: "compression-text" },
      h("label", { for: "compression-switch", class: "compression-label" }, icon("compress"), t("compression.label")),
      hint,
      images,
      h("p", { class: "hint compression-status", "aria-live": "polite" })),
    toggle);
}

// The note on where to see what compression saves: only while the switch is on, and
// then part of the switch's description.
function showImagesNote(toggle) {
  document.getElementById("compression-images").hidden = !toggle.checked;
  toggle.setAttribute("aria-describedby", toggle.checked ? "compression-hint compression-images" : "compression-hint");
}

// Show the engine's state on the switch. While the VM is off it waits (marked
// data-waiting), and refreshVm reads it again when the VM runs.
async function readCompressionSwitch() {
  // A write the switch started finishes first: the read shows its result.
  await compressionWrite;
  const toggle = document.getElementById("compression-switch");
  if (!toggle) return;
  const hint = document.getElementById("compression-hint");
  const status = toggle.parentElement.querySelector(".compression-status");
  const say = (cls, text) => { status.className = `${cls} compression-status`; status.textContent = text; };
  if ((await compressionAccess()) !== "read") {
    toggle.dataset.waiting = "";
    say("hint", t("offline.vmTitle"));
    return;
  }
  delete toggle.dataset.waiting;
  say("hint", "");
  try {
    const state = await API.compression();
    toggle.checked = state?.mode === "idle";
    hint.textContent = compressionHint(state);
    showImagesNote(toggle);
    toggle.disabled = false;
  } catch (e) {
    say("error-text", String(e));
  }
}

// ---- notices ----

// A notice in the corner, next to the toasts. It stays until one of its buttons is used.
function notice(id, title, text, buttons) {
  document.getElementById(id)?.remove();
  const status = h("p", { class: "error-text" });
  const el = h("section", { class: "notice", id, "aria-label": title },
    h("strong", { class: "notice-title", text: title }),
    h("p", { text }),
    status,
    h("div", { class: "buttons-row" }, buttons));
  document.getElementById("toasts").append(el);
  return { status, close: () => el.remove() };
}

// ---- the podbag command for terminals ----

// Where Install puts the command: a link (macOS, Linux) or the app's folder in Path.
function terminalCommandWhere(c) {
  return t(navigator.userAgent.includes("Windows") ? "cli.wherePath" : "cli.whereLink", { path: c.target });
}

// A packaged app that finds no podbag for terminals offers its own, until "Not now".
async function showTerminalCommandNotice() {
  try {
    S.cli = await API.terminalCommand();
  } catch {
    S.cli = null;
  }
  renderTerminalCommand();
  const c = S.cli;
  if (!c || c.state !== "missing" || c.problem || S.settings.cliNoticeDismissed) return;
  const later = button(null, t("cli.later"), () => {
    n.close();
    S.settings.cliNoticeDismissed = true;
    saveSettings();
    toast(t("cli.laterNote"));
  });
  const install = button("shell", t("cli.install"), async () => {
    if (await installTerminalCommand(install, n.status)) n.close();
  }, "primary");
  const n = notice("cli-notice", t("cli.title"), `${t("cli.text")} ${terminalCommandWhere(c)}`, [later, install]);
}

// Make the command; `btn` shows the work and `status` an error. True when it was made
// (false also when the password was not given).
async function installTerminalCommand(btn, status) {
  const label = btn.querySelector("span");
  btn.disabled = true;
  label.textContent = t("cli.installing");
  status.textContent = "";
  try {
    const done = await API.installTerminalCommand();
    if (!done) return false;
    S.cli = done;
    document.getElementById("cli-notice")?.remove();
    renderTerminalCommand();
    toast(t("cli.done"));
    return true;
  } catch (e) {
    status.textContent = t("cli.failed", { error: String(e) });
    return false;
  } finally {
    btn.disabled = false;
    label.textContent = t("cli.install");
  }
}

// A separate podbag for the terminal that is older than the app (the app was updated
// another way): offer to bring it to the app's version. Update in Settings does the same.
function showTerminalUpdateNotice() {
  const u = S.updates || {};
  const old = document.getElementById("cli-update-notice");
  if (!u.installer || !u.terminalOlder || S.settings.cliUpdateDismissed === u.app) {
    old?.remove();
    return;
  }
  if (old || upgrading) return;
  const later = button(null, t("cli.later"), () => {
    n.close();
    S.settings.cliUpdateDismissed = u.app;
    saveSettings();
  });
  const update = button("update", t("cliUpdate.update"), async () => {
    update.disabled = true;
    update.querySelector("span").textContent = t("update.installing");
    let r;
    try {
      r = await API.upgrade(u.app);
    } catch (e) {
      r = { ok: false, output: String(e) };
    }
    n.close();
    if (r.ok) {
      toast(t("cliUpdate.done", { version: u.app }));
    } else {
      // The password or an error: Updates shows the command and what it printed.
      S.upgrade = r;
      await openUpdates();
    }
    await loadUpdates("never");
  }, "primary");
  const n = notice("cli-update-notice", t("cliUpdate.title"),
    t("cliUpdate.text", { terminal: u.terminalVersion, app: u.app, path: u.terminal }), [later, update]);
}

// Settings: what terminals run, and Install when they find no podbag.
function renderTerminalCommand() {
  const section = document.getElementById("terminal-command");
  if (!section) return;
  const c = S.cli;
  section.hidden = !c;
  if (!c) return;
  section.querySelector(".setting-value").textContent = t(`cli.state.${c.state}`);
  const status = h("p", { class: "error-text", role: "status" });
  let action = null;
  if (c.state === "missing" && c.problem) {
    action = h("p", { class: "error-text", text: t(`cli.problem.${c.problem}`) });
  } else if (c.state === "missing") {
    const install = button("shell", t("cli.install"), () => installTerminalCommand(install, status), "primary");
    install.dataset.settingSearch = "cli.install";
    action = [h("p", { class: "hint", text: terminalCommandWhere(c) }), h("div", { class: "buttons-row" }, install), status];
  }
  put(section.querySelector(".setting-body"),
    h("p", { class: "hint", text: t(`cli.about.${c.state}`) }),
    c.path ? h("dl", { class: "fields" }, field(t("cli.path"), c.path, "mono")) : null,
    action);
}

// ---- settings: updates ----

let checking = false;
let upgrading = false;

// When the window opens: the last check, and a new one when that is a day old.
async function loadUpdates(check) {
  if (check === "now") checking = true;
  renderUpdates();
  try {
    S.updates = await API.updates(check);
  } catch (e) {
    S.updates = { ...(S.updates || {}), error: String(e) };
  }
  checking = false;
  renderUpdates();
  renderStatusbar();
  showTerminalUpdateNotice();
}

function renderUpdates() {
  const section = document.getElementById("updates");
  if (!section) return;
  const u = S.updates || {};
  const status = checking ? t("update.checking")
    : u.error ? u.error
      : u.newer ? t("update.available", { version: u.latest })
        : u.latest ? t("update.upToDate") : "";
  const check = button("refresh", t("update.check"), () => loadUpdates("now"));
  check.dataset.settingSearch = "update.check";
  check.disabled = checking || upgrading;
  const install = button("update", upgrading ? t("update.installing") : t("update.install"), runUpgrade, "primary");
  install.dataset.settingSearch = "update.install";
  install.disabled = !u.newer || checking || upgrading;
  const shown = u.installer ? u.app : u.engine;
  section.querySelector(".setting-value").textContent = u.newer && u.latest ? t("status.available", { version: u.latest }) : shown || "";
  put(section.querySelector(".setting-body"),
    h("dl", { class: "fields" },
      field(t("settings.appVersion"), u.app),
      field(t("settings.engineVersion"), u.engine),
      u.terminal ? field(t("settings.terminalVersion"), `${u.terminalVersion || "?"} · ${u.terminal}`) : null,
      field(t("settings.latest"), u.latest),
      field(t("settings.checked"), u.checked ? ago(u.checked) : t("settings.never"))),
    status ? h("p", { class: u.error ? "error-text" : u.newer ? "update-note" : "hint", role: "status", text: status }) : null,
    h("div", { class: "buttons-row" }, check, install),
    upgradeResult(),
    appNote(u));
}

// What Update does with the app: a packaged app installs its new package (with its own
// podbag); a build of its own cannot.
function appNote(u) {
  if (u.installer) {
    return h("p", { class: "hint", text: u.terminal ? t("update.appSelfTerminal") : t("update.appSelf") });
  }
  return h("div", {},
    h("p", { class: "hint", text: t("update.appManual") }),
    h("div", { class: "buttons-row" }, button("globe", t("update.openReleases"), async () => {
      try {
        await API.openReleases();
      } catch (e) {
        toast(String(e), true);
      }
    })));
}

function upgradeResult() {
  const r = S.upgrade;
  if (!r || r.ok) return null;
  if (r.needsAdmin) {
    const windows = S.engine && S.engine.platform === "windows";
    const u = S.updates || {};
    // The terminal's podbag waits for the password; the app can still update now.
    const appNow = u.appNewer && u.installer
      ? button("update", t("update.appNow"), installApp, "primary")
      : null;
    if (appNow) appNow.disabled = upgrading;
    return h("div", { class: "upgrade-result" },
      h("p", { text: windows ? t("update.needsAdminWindows") : t("update.needsAdmin") }),
      codeLine(r.command),
      h("div", { class: "buttons-row" },
        windows ? null : button("shell", t("update.openTerminal"), async () => {
          try {
            await API.openTerminal("upgrade");
          } catch {
            toast(t("terminal.none"), true);
          }
        }),
        appNow));
  }
  return h("div", { class: "upgrade-result" },
    h("p", { class: "error-text", text: t("update.failed") }),
    r.output ? h("pre", { class: "json", text: r.output }) : null);
}

// Update: the terminal's podbag first (to the same version), then the app, which
// starts again.
async function runUpgrade() {
  const u = S.updates || {};
  upgrading = true;
  S.upgrade = null;
  renderUpdates();
  if (u.cliNewer) {
    try {
      S.upgrade = await API.upgrade(u.latest);
    } catch (e) {
      S.upgrade = { ok: false, output: String(e) };
    }
    if (!S.upgrade.ok) {
      upgrading = false;
      renderUpdates();
      return;
    }
  }
  if (u.appNewer && u.installer) {
    await installApp();
    return;
  }
  upgrading = false;
  toast(t("update.done"));
  await loadUpdates("never");
}

// Install the app's new package. On success the app ends and the new one starts.
async function installApp() {
  upgrading = true;
  renderUpdates();
  try {
    await API.updateApp((S.updates || {}).latest);
    toast(t("update.restarting"));
  } catch (e) {
    upgrading = false;
    S.upgrade = { ok: false, output: String(e) };
    renderUpdates();
  }
}

async function saveSettings() {
  try {
    await API.setSettings(S.settings);
  } catch (e) {
    toast(String(e), true);
  }
}

function renderAll() {
  renderNav();
  renderEngine();
  renderVmCard();
  renderStatusbar();
  renderBanner();
  applySidebar();
  renderHistory();
  renderTopbar();
  renderMain(true);
}

function applyTheme() {
  const v = S.settings.theme === "light" || S.settings.theme === "dark" ? S.settings.theme : "light dark";
  document.querySelector('meta[name="color-scheme"]').content = v;
}

// The docker command banner (filled in when known).
function renderBanner() {
  const el = document.getElementById("banner");
  if (el) el.hidden = true;
}

// Tools at the right of the top bar.
function renderTopbar() {
  const box = document.getElementById("topbar-tools");
  if (!box || windowBar) return;
  const settings = iconButton("settings", t("nav.settings"), () => go("settings"));
  box.replaceChildren(settings);
}

// ---- start ----

let readyDone;
const ready = new Promise((r) => { readyDone = r; });

// The VM card polls the VM's state, but only while the window shows.
let vmTimer = null;
function pollVm(on) {
  clearInterval(vmTimer);
  vmTimer = on ? setInterval(() => { if (!document.hidden && hasVm()) refreshVm(); }, 5000) : null;
}

function onKey(e) {
  const mod = IS_MAC ? e.metaKey : e.ctrlKey;
  const key = e.key.toLowerCase();
  const inDialog = document.querySelector("dialog[open]");
  if (IS_MAC ? (e.ctrlKey && e.metaKey && key === "s") : (e.ctrlKey && !e.shiftKey && key === "b")) {
    e.preventDefault();
    toggleSidebar();
  } else if (mod && (key === "f" || key === "k") && !e.shiftKey) {
    const s = document.querySelector(".search");
    if (s) { e.preventDefault(); s.focus(); }
  } else if ((IS_MAC && e.metaKey && e.key === "[") || (!IS_MAC && e.altKey && e.key === "ArrowLeft")) {
    if (!inDialog) { e.preventDefault(); travel(-1); }
  } else if ((IS_MAC && e.metaKey && e.key === "]") || (!IS_MAC && e.altKey && e.key === "ArrowRight")) {
    if (!inDialog) { e.preventDefault(); travel(1); }
  } else if (e.key === "Escape" && S.detail && !inDialog) {
    closeDetail();
  } else {
    extraKeys(e);
  }
}

// Shortcuts of later parts (the pull panel).
function extraKeys() {}

async function init() {
  S.connection = await API.connection();
  const view = sessionStorage.getItem("engine-view");
  if ([...KINDS, "settings", "kubernetes", "migration", "models", "extensions"].includes(view)) S.view = view;
  const kind = sessionStorage.getItem("engine-resource");
  if (KUBE_KINDS.includes(kind)) S.kubeKind = kind;
  S.kubeExpanded = S.view === "kubernetes";
  sessionStorage.removeItem("engine-view");
  sessionStorage.removeItem("engine-resource");
  API.contexts().then((names) => {
    S.contexts = names;
    if (S.view === "settings" && document.getElementById("settings-engine-picker")) renderMain(true);
  }).catch(() => {});
  try {
    S.settings = { ...S.settings, ...(await API.getSettings()) };
  } catch { /* the defaults */ }
  applyTheme();
  await I18N.load(S.settings.language);
  try { setInstalledPractice(await API.volumeToolsLab("summary")); }
  catch { /* The Training page can retry the library. */ }
  // New Training modules from podbag.dev appear without an app update.
  S.trainingSyncedAt = Date.now();
  API.volumeToolsLab("sync").then(records => {
    setInstalledPractice(records);
    renderNav();
  }).catch(() => { /* Offline: the installed modules stay. */ });
  setupWindowBar();
  setupKubeNav();
  setupTrainingNav();
  setupSidebar();
  renderAll();
  refreshKubeCounts();
  refreshKubeOpenShift();
  remember();
  API.on("changed", refreshSoon);
  API.on("live", (v) => { S.live = !!v; renderLive(); });
  API.on("logs", onLogs);
  API.on("pull-progress", (line) => { if (pullLine) pullLine(line); });
  API.on("shown", () => { connect(false); loadUpdates("due"); });
  API.on("vm-status", (vm) => {
    if (S.engine?.platform !== "macos" || !S.engine.vm || S.engine.vm === vm) return;
    S.engine.vm = vm;
    if (vm === "stopped") {
      S.engine.reachable = false;
      S.loaded = false;
      clearTimeout(refreshTimer);
      API.watch(false);
      renderNav();
    } else if (!S.engine.reachable && !document.hidden && !vmBusy) {
      connect(false);
      return;
    }
    refreshVm();
    renderMain();
  });
  API.on("engine-changed", () => connect(false));
  API.on("open-updates", openUpdates);
  API.on("open-settings", () => go("settings"));
  await API.on("docker-switching", dockerSwitching);
  await API.on("docker-changed", dockerChanged);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) return API.watch(false);
    if (S.engine && S.engine.reachable) { API.watch(true); refresh(); }
    refreshVm();
  });
  document.addEventListener("keydown", onKey);
  // Mouse buttons 4 and 5: back and forward.
  document.addEventListener("mouseup", (e) => {
    if (e.button === 3) travel(-1);
    else if (e.button === 4) travel(1);
  });
  // Without events, read the lists every few seconds while the window shows.
  setInterval(() => {
    if (!S.live && !document.hidden && S.engine && S.engine.reachable) refresh();
  }, 3000);
  showFirstLaunchStarPrompt().then(showCompressionPrompt).then(showTerminalCommandNotice);
  pollVm(true);
  loadUpdates("due");
  await connect(false);
  readyDone();
}

// For the preview page (desktop/preview), which shows each view with a mocked API.
window.podbag = {
  ready, go, openDetail, run: (image) => openRunDialog(image), remove: (kind, i) => remove(kind, S.lists[kind][i]), pull, prune, state: S,
  toggleSidebar, openSettingsSection, openUpdates, switchEngine, applyVm: () => applyVm(document.querySelector(".vm-form")), runUpgrade,
};

// images.js starts the app, once its parts are in place.
