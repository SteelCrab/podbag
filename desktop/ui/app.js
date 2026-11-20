// podbag's window: lists of containers, images, volumes and networks, their details,
// logs and actions, and the settings. Plain DOM, no framework. Data from the engine is
// always set as text, never as HTML.
"use strict";

const KINDS = ["containers", "images", "volumes", "networks"];
const t = (key, vars) => I18N.t(key, vars);

const S = {
  view: "containers", // an engine resource, "kubernetes" or "settings"
  kubeKind: "pods",
  kubeContext: sessionStorage.getItem("kube-context") || "",
  connection: null,
  contexts: [],
  listErrors: {},
  kubeExpanded: false,
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
  autoCollapsed: false, // the window is too narrow for the sidebar
  docker: null, // where the docker command goes
};

const IS_MAC = /Mac/.test(navigator.platform || navigator.userAgent);
const SIDEBAR = { width: 216, min: 160, max: 360 };

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
  renderSwitch();
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
  if (S.view === "settings") renderVmSettings();
}

// Switch this window to podbag or Docker; the TUI reads the same choice.
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

// Pull progress goes here while a pull runs; images.js opens the pull panel.
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
  windowBar.prepend(document.querySelector(".brand"));
  document.getElementById("engine").remove();
  const reload = iconButton("refresh", "", () => {
    if (S.view === "kubernetes") return kubePanel.reload();
    return S.engine?.reachable ? refresh() : connect(false);
  });
  const settings = iconButton("settings", "", () => go("settings"));
  actions.append(reload, settings);
  document.getElementById("nav-bottom").remove();
  windowBar.querySelector(".topbar-space").setAttribute("data-tauri-drag-region", "");
  windowBar.querySelector(".brand").setAttribute("data-tauri-drag-region", "deep");
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
  S.view = entry.view;
  if (entry.kubeKind) { S.kubeKind = entry.kubeKind; S.kubeExpanded = true; }
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
  renderMain(true);
  remember();
}

// Keep the disclosure and child buttons mounted when the engine refreshes its lists.
function setupKubeNav() {
  const open = h("button", { type: "button", id: "nav-kube-open", class: "nav-item", onclick: () => go("kubernetes") },
    icon("kubernetes"), h("span", { class: "label", text: t("nav.kubernetes") }));
  const toggle = h("button", {
    type: "button", id: "nav-kube-toggle", class: "icon-button kube-toggle",
    "aria-label": t("kube.resources"), title: t("kube.resources"), "aria-controls": "nav-kube-children",
    onclick: () => { S.kubeExpanded = !S.kubeExpanded; updateKubeNav(); },
  }, icon("back"));
  const children = h("ul", { class: "nav kube-subnav", id: "nav-kube-children", "aria-label": t("kube.resources") },
    KUBE_KINDS.map((kind) => h("li", {}, h("button", {
      type: "button", class: "nav-item", id: `nav-kube-${kind}`, onclick: () => go("kubernetes", kind),
    }, t(`kube.${kind}`)))));
  document.getElementById("nav-kubernetes").replaceChildren(
    h("li", {}, h("div", { class: "kube-nav-header" }, open, toggle), children));
}

function updateKubeNav() {
  const active = S.view === "kubernetes";
  document.getElementById("nav-kube-toggle").setAttribute("aria-expanded", String(S.kubeExpanded));
  document.getElementById("nav-kube-children").hidden = !S.kubeExpanded;
  for (const [id, current] of [
    ["nav-kube-open", active && !S.kubeExpanded],
    ...KUBE_KINDS.map((kind) => [`nav-kube-${kind}`, active && S.kubeExpanded && S.kubeKind === kind]),
  ]) {
    const button = document.getElementById(id);
    if (current) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
}

function renderNav() {
  const nav = document.getElementById("nav");
  const item = (view, label, count) => h("li", {}, h("button", {
    type: "button", class: "nav-item", "aria-current": S.view === view ? "page" : null, onclick: () => go(view),
    title: label,
  }, icon(view), h("span", { class: "label", text: label }), count == null ? null : h("span", { class: "count", text: count })));
  nav.replaceChildren(...KINDS.map((k) => item(k, t(`nav.${k}`), S.loaded ? S.lists[k].length : null)));
  updateKubeNav();
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

// "This window: podbag / Docker". `where` names the radio group (sidebar or settings).
function engineChoice(where) {
  const name = S.connection.name;
  const option = (value, text) => h("option", { value, text });
  const select = h("select", { class: "engine-picker", id: where === "sidebar" ? "engine-picker" : "settings-engine-picker",
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

function renderSwitch() {
  const name = S.engine?.engineName || S.connection.name;
  const rail = iconButton("engine", t("switch.current", { name }), () => {
    go("settings");
    document.getElementById("settings-engine-picker")?.focus();
  }, "switch-rail");
  put(document.getElementById("engine-switch"),
    h("label", { class: "switch-label", for: "engine-picker", text: t("switch.label") }), engineChoice("sidebar"), rail);
}

// ---- the sidebar's width, collapse and divider ----

const narrowWindow = window.matchMedia("(max-width: 720px)");

function sidebarCollapsed() {
  return S.settings.sidebarCollapsed || S.autoCollapsed;
}

function applySidebar() {
  const app = document.getElementById("app");
  const collapsed = sidebarCollapsed();
  const width = clampWidth(S.settings.sidebarWidth);
  app.style.setProperty("--sidebar-w", `${width}px`);
  app.classList.toggle("collapsed", collapsed);
  const splitter = document.getElementById("splitter");
  splitter.setAttribute("aria-valuenow", String(width));
  splitter.setAttribute("aria-label", t("sidebar.resize"));
  splitter.hidden = collapsed;
  const toggle = document.getElementById("sidebar-toggle");
  const label = collapsed ? t("sidebar.show") : t("sidebar.hide");
  if (!toggle.firstChild) toggle.append(icon("sidebar"));
  toggle.setAttribute("aria-expanded", collapsed ? "false" : "true");
  toggle.setAttribute("aria-label", label);
  toggle.title = `${label} (${IS_MAC ? "⌃⌘S" : "Ctrl+B"})`;
}

const clampWidth = (w) => Math.min(SIDEBAR.max, Math.max(SIDEBAR.min, Math.round(Number(w) || SIDEBAR.width)));

let saveSidebarTimer = null;
function saveSidebarSoon() {
  clearTimeout(saveSidebarTimer);
  saveSidebarTimer = setTimeout(() => API.setSettings(S.settings).catch(() => {}), 300);
}

function setSidebarWidth(w, save) {
  S.settings.sidebarWidth = clampWidth(w);
  applySidebar();
  if (save) saveSidebarSoon();
}

function toggleSidebar() {
  if (sidebarCollapsed()) {
    S.autoCollapsed = false;
    S.settings.sidebarCollapsed = false;
  } else if (narrowWindow.matches) {
    // Too narrow: hide it for now, without changing the saved choice.
    S.autoCollapsed = true;
  } else {
    S.settings.sidebarCollapsed = true;
  }
  applySidebar();
  saveSidebarSoon();
}

function setupSidebar() {
  const splitter = document.getElementById("splitter");
  const app = document.getElementById("app");
  let drag = null;
  splitter.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    splitter.setPointerCapture(e.pointerId);
    drag = { x: e.clientX, width: clampWidth(S.settings.sidebarWidth) };
    app.classList.add("resizing");
  });
  splitter.addEventListener("pointermove", (e) => {
    if (!drag || !splitter.hasPointerCapture(e.pointerId)) return;
    const dir = document.documentElement.dir === "rtl" ? -1 : 1;
    setSidebarWidth(drag.width + (e.clientX - drag.x) * dir, false);
  });
  const end = (e) => {
    if (!drag) return;
    drag = null;
    app.classList.remove("resizing");
    if (splitter.hasPointerCapture(e.pointerId)) splitter.releasePointerCapture(e.pointerId);
    saveSidebarSoon();
  };
  splitter.addEventListener("pointerup", end);
  splitter.addEventListener("pointercancel", end);
  splitter.addEventListener("lostpointercapture", end);
  splitter.addEventListener("dblclick", () => setSidebarWidth(SIDEBAR.width, true));
  splitter.addEventListener("keydown", (e) => {
    const step = e.shiftKey ? 40 : 10;
    const w = clampWidth(S.settings.sidebarWidth);
    const keys = {
      ArrowLeft: w - step, ArrowRight: w + step, ArrowUp: w + step, ArrowDown: w - step,
      Home: SIDEBAR.min, End: SIDEBAR.max,
    };
    if (e.key === "Enter") {
      e.preventDefault();
      toggleSidebar();
      document.getElementById("sidebar-toggle").focus();
    } else if (e.key in keys) {
      e.preventDefault();
      setSidebarWidth(keys[e.key], true);
    }
  });
  document.getElementById("sidebar-toggle").addEventListener("click", toggleSidebar);
  document.getElementById("nav-back").addEventListener("click", () => travel(-1));
  document.getElementById("nav-forward").addEventListener("click", () => travel(1));
  // Narrow windows collapse it; it comes back when there is room.
  const onWidth = () => {
    S.autoCollapsed = narrowWindow.matches;
    applySidebar();
  };
  narrowWindow.addEventListener("change", onWidth);
  S.autoCollapsed = narrowWindow.matches;
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
    h("button", { type: "button", class: "icon-button vm-rail", title: `${t("vm.title")}: ${stateText}`, "aria-label": `${t("vm.title")}: ${stateText}`, onclick: () => go("settings") },
      icon("vm"), h("span", { class: `dot ${running ? "dot-on" : ""}`, "aria-hidden": "true" })));
}

// ---- the status bar ----

function renderStatusbar() {
  const bar = document.getElementById("statusbar");
  if (!bar) return;
  const u = S.updates;
  const left = [];
  if (u && u.engine) {
    left.push(h("button", { type: "button", class: "status-item", title: t("settings.updates"), onclick: openUpdates }, `podbag ${u.engine}`));
  }
  if (u && u.newer && u.latest) {
    left.push(h("button", { type: "button", class: "status-item status-update", onclick: openUpdates },
      icon("update"), t("status.available", { version: u.latest })));
  }
  left.push(h("button", { type: "button", class: "status-item", title: t("status.terminal"), onclick: openPlainTerminal }, icon("shell"), t("status.terminal")));
  const right = [];
  const [cls, text] = S.vm && hasVm() ? [S.vm.state === "running" ? "ok" : "off", `${t("vm.title")}: ${t(`vm.state.${vmStateKey(S.vm.state)}`)}`] : engineState();
  const name = (S.engine && S.engine.engineName) || "podbag";
  right.push(h("span", { class: `status-item engine-${cls}` }, h("span", { class: "dot", "aria-hidden": "true" }),
    S.vm && hasVm() ? text : `${name}: ${text}`));
  if (S.loaded) {
    const total = S.lists.containers.length;
    const running = S.lists.containers.filter((c) => c.State === "running").length;
    right.push(h("span", { class: "status-item", text: t("status.running", { n: running, total }) }));
  }
  if (S.vm && S.vm.memoryTotalMiB) {
    right.push(h("span", { class: "status-item", title: t("vm.memory"), text: `${mib(S.vm.memoryUsedMiB || 0)} / ${mib(S.vm.memoryTotalMiB)}` }));
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
  if (S.view !== "settings") go("settings");
  // The settings page draws after the language names load.
  const show = () => {
    const el = document.getElementById("updates");
    if (el) el.scrollIntoView({ block: "start", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
    else setTimeout(show, 50);
  };
  show();
}

function dockerCommandItem() { return null; }

// ---- the main area ----

let shownView = null;
let kubePanel;

function renderMain(fresh) {
  const main = document.getElementById("main");
  if (S.view === "kubernetes") {
    if (fresh || shownView !== S.view) kubePanel = renderKubernetes(main);
    shownView = S.view;
    return;
  }
  kubePanel = null;
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
  const cmd = isPodbag() ? "podbag" : "docker";
  const commands = {
    containers: `${cmd} run -d --name web -p 8080:80 nginx:alpine`,
    volumes: `${cmd} volume create data`,
    networks: `${cmd} network create app`,
  };
  return h("div", { class: "empty" },
    h("div", { class: "empty-icon" }, icon(kind)),
    h("h2", { text: t(`empty.${kind}.title`) }),
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
  put(main,
    h("header", { class: "toolbar" }, h("h1", { text: t("settings.title") })),
    h("div", { class: "content settings" },
      h("section", { class: "card" },
        h("label", { class: "setting-label", for: "language", text: t("settings.language") }), lang),
      h("section", { class: "card" },
        h("fieldset", {}, h("legend", { class: "setting-label", text: t("settings.theme") }), h("div", { class: "radios" }, themes))),
      h("section", { class: "card", id: "engine-settings" },
        h("h2", { class: "setting-label", text: t("settings.engine") }),
        h("div", { class: "setting-row" }, h("span", { class: "secondary-text", text: t("switch.label") }), engineChoice("settings")),
        h("p", { class: "hint", text: t("settings.engineNote") }),
        h("dl", { class: "fields" },
          field(t("settings.socket"), e.endpoint, "mono"),
          field(t("settings.version"), e.version)),
        dockerSettings()),
      h("section", { class: "card", id: "vm-settings", hidden: true }),
      h("section", { class: "card", id: "updates" }),
      h("section", { class: "card" },
        h("h2", { class: "setting-label", text: t("settings.about") }),
        h("p", { text: t("settings.aboutText") }))));
  renderVmSettings(true);
  renderUpdates();
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
  const box = document.getElementById("vm-settings");
  if (!box) return;
  const vm = S.vm;
  if (!vm || !hasVm()) {
    box.hidden = true;
    box.replaceChildren();
    return;
  }
  box.hidden = false;
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
    h("h2", { class: "setting-label", text: t("settings.vm") }),
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

function vmForm(vm) {
  const l = vm.limits || {};
  const over = new Map((vm.overriddenBy || []).map((name) => [ENV_FIELD[name], name]));
  const input = (id, label, props, hint) => {
    const value = h("input", { id, type: "number", inputmode: "decimal", required: true, ...props });
    const slider = id === "vm-idle" ? null : h("input", { type: "range", "aria-label": label, ...props });
    value.oninput = () => { vmFormDirty = true; if (slider) slider.value = value.value; };
    if (slider) slider.oninput = () => { vmFormDirty = true; value.value = slider.value; };
    const variable = over.get(id.slice(3));
    return h("div", { class: "form-field" }, h("label", { for: id, text: label }), value, slider,
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
  if (!Object.keys(change).length && !vm.pending) {
    toast(t("settings.vmSame"));
    return;
  }
  if (!Object.keys(change).length) change.cpus = cpus; // only to restart with them
  let restart = false;
  const inUse = vm.inUse || {};
  const differs = cpus !== inUse.cpus || memoryMiB !== inUse.memoryMiB || idleSeconds !== inUse.idleSeconds;
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
function chooseDialog({ title, text, buttons }) {
  const d = h("dialog", { class: "choose", "aria-labelledby": "choose-title" },
    h("form", { method: "dialog" },
      h("h2", { id: "choose-title", text: title }),
      h("p", { text }),
      h("div", { class: "buttons" }, buttons.map((b, i) => h("button", {
        value: b.value, class: `button ${b.cls || ""}`, autofocus: i === 0, text: b.label,
      })))));
  document.body.append(d);
  d.showModal();
  return new Promise((resolve) => {
    d.addEventListener("close", () => {
      resolve(d.returnValue || "");
      d.remove();
    }, { once: true });
  });
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
}

function renderUpdates() {
  const box = document.getElementById("updates");
  if (!box) return;
  const u = S.updates || {};
  const status = checking ? t("update.checking")
    : u.error ? u.error
      : u.newer ? t("update.available", { version: u.latest })
        : u.latest ? t("update.upToDate") : "";
  const check = button("refresh", t("update.check"), () => loadUpdates("now"));
  check.disabled = checking || upgrading;
  const install = button("update", upgrading ? t("update.installing") : t("update.install"), runUpgrade, "primary");
  install.disabled = !u.newer || checking || upgrading;
  put(box,
    h("h2", { class: "setting-label", text: t("settings.updates") }),
    h("dl", { class: "fields" },
      field(t("settings.engineVersion"), u.engine),
      field(t("settings.appVersion"), u.app),
      field(t("settings.latest"), u.latest),
      field(t("settings.checked"), u.checked ? ago(u.checked) : t("settings.never"))),
    status ? h("p", { class: u.error ? "error-text" : u.newer ? "update-note" : "hint", role: "status", text: status }) : null,
    h("div", { class: "buttons-row" }, check, install),
    upgradeResult(),
    h("p", { class: "hint", text: t("update.appNote") }));
}

function upgradeResult() {
  const r = S.upgrade;
  if (!r || r.ok) return null;
  if (r.needsAdmin) {
    const windows = S.engine && S.engine.platform === "windows";
    return h("div", { class: "upgrade-result" },
      h("p", { text: windows ? t("update.needsAdminWindows") : t("update.needsAdmin") }),
      codeLine(r.command),
      windows ? null : h("div", { class: "buttons-row" }, button("shell", t("update.openTerminal"), async () => {
        try {
          await API.openTerminal("upgrade");
        } catch {
          toast(t("terminal.none"), true);
        }
      })));
  }
  return h("div", { class: "upgrade-result" },
    h("p", { class: "error-text", text: t("update.failed") }),
    r.output ? h("pre", { class: "json", text: r.output }) : null);
}

async function runUpgrade() {
  upgrading = true;
  S.upgrade = null;
  renderUpdates();
  try {
    S.upgrade = await API.upgrade();
  } catch (e) {
    S.upgrade = { ok: false, output: String(e) };
  }
  upgrading = false;
  if (S.upgrade.ok) {
    toast(t("update.done"));
    await loadUpdates("never");
  } else {
    renderUpdates();
  }
}

async function saveSettings() {
  try {
    await API.setSettings(S.settings);
  } catch (e) {
    toast(String(e), true);
  }
  await I18N.load(S.settings.language);
  setupKubeNav();
  renderAll();
}

function renderAll() {
  renderNav();
  renderEngine();
  renderSwitch();
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
  if ([...KINDS, "settings", "kubernetes"].includes(view)) S.view = view;
  const kind = sessionStorage.getItem("engine-resource");
  if (KUBE_KINDS.includes(kind)) S.kubeKind = kind;
  S.kubeExpanded = S.view === "kubernetes";
  sessionStorage.removeItem("engine-view");
  sessionStorage.removeItem("engine-resource");
  API.contexts().then((names) => {
    S.contexts = names;
    if (document.getElementById("engine-picker")) {
      renderSwitch();
      if (S.view === "settings") renderMain(true);
    }
  }).catch(() => {});
  try {
    S.settings = { ...S.settings, ...(await API.getSettings()) };
  } catch { /* the defaults */ }
  applyTheme();
  await I18N.load(S.settings.language);
  setupWindowBar();
  setupKubeNav();
  setupSidebar();
  renderAll();
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
  API.on("docker-changed", refreshDocker);
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
  pollVm(true);
  loadUpdates("due");
  await connect(false);
  readyDone();
}

// For the preview page (desktop/preview), which shows each view with a mocked API.
window.podbag = {
  ready, go, openDetail, remove: (kind, i) => remove(kind, S.lists[kind][i]), pull, prune, state: S,
  toggleSidebar, openUpdates, switchEngine, applyVm: () => applyVm(document.querySelector(".vm-form")), runUpgrade,
};

// images.js starts the app, once its parts are in place.
