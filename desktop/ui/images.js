// The pull panel, the docker command (banner, status bar, settings) and the import from
// Docker. Uses app.js's helpers. Data from the engine is always set as text.
"use strict";

// ---- the pull panel ----

const P = {
  dialog: null,
  location: "dockerhub",
  hosts: loadHosts(), // user-supplied registry hosts
  locations: [],
  results: [],
  active: 0,
  step: "search", // search, tag, login, host, pulling, done
  chosen: null, // a search result, for the tag step
  searchSupported: true,
  searchError: null,
  lines: [],
  busy: false,
};

function registryIcon(id) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "registry-icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(svg.namespaceURI, "use");
  use.setAttribute("href", `registry-icons.svg#${id}`);
  svg.append(use);
  return svg;
}

function loadHosts() {
  try {
    return JSON.parse(localStorage.getItem("podbag.hosts") || "{}") || {};
  } catch {
    return {};
  }
}

function saveHosts() {
  try {
    localStorage.setItem("podbag.hosts", JSON.stringify(P.hosts));
  } catch { /* only a convenience */ }
}

const currentLocation = () => P.locations.find((l) => l.id === P.location) || { id: "dockerhub", label: "Docker Hub", host: "docker.io" };

async function loadLocations() {
  try {
    P.locations = await API.registries(Object.entries(P.hosts));
  } catch (e) {
    P.locations = [];
    toast(String(e), true);
  }
}

// A reference the user pasted whole: with a tag, a digest or a registry host.
const looksFull = (q) => /[:@]/.test(q) || /^[^/]+[.:][^/]*\//.test(q) || q.startsWith("localhost/");

async function openPullPanel() {
  if (S.engine && !S.engine.reachable) return;
  if (!P.dialog) buildPullPanel();
  P.step = "search";
  P.results = [];
  P.active = 0;
  P.lines = [];
  const input = P.dialog.querySelector(".pull-input");
  input.value = "";
  P.dialog.showModal();
  input.focus();
  await loadLocations();
  renderPullPanel();
}

function buildPullPanel() {
  const input = h("input", {
    type: "text", class: "pull-input", role: "combobox", "aria-autocomplete": "list", "aria-expanded": "false",
    "aria-controls": "pull-results", autocomplete: "off", autocapitalize: "off", spellcheck: "false",
    oninput: onPullInput, onkeydown: onPullKey,
  });
  const locButton = h("button", { type: "button", class: "loc-button", "aria-haspopup": "true", "aria-expanded": "false", onclick: toggleLocMenu });
  P.dialog = h("dialog", { class: "pull-panel", id: "pull-panel" },
    h("div", { class: "pull-head" }, icon("search"), input, locButton),
    h("div", { class: "loc-menu", hidden: true }),
    h("div", { class: "pull-body" }),
    h("footer", { class: "pull-foot" }));
  // Esc steps back first (tag, log in, a menu), then closes.
  P.dialog.addEventListener("cancel", (e) => {
    const menu = P.dialog.querySelector(".loc-menu");
    if (!menu.hidden) {
      e.preventDefault();
      closeLocMenu();
    } else if (["tag", "login", "host"].includes(P.step) && !P.busy) {
      e.preventDefault();
      P.step = "search";
      renderPullPanel();
      P.dialog.querySelector(".pull-input").focus();
    }
  });
  P.dialog.addEventListener("click", (e) => {
    if (e.target === P.dialog) P.dialog.close();
  });
  document.body.append(P.dialog);
}

let searchTimer = null;
function onPullInput() {
  P.active = 0;
  P.step = "search";
  clearTimeout(searchTimer);
  const q = P.dialog.querySelector(".pull-input").value.trim();
  if (!q || looksFull(q) || P.location !== "dockerhub" || !P.searchSupported) {
    P.results = [];
    renderPullPanel();
    return;
  }
  renderPullPanel();
  searchTimer = setTimeout(async () => {
    try {
      const r = await API.search(q);
      if (P.location !== "dockerhub" || P.dialog.querySelector(".pull-input").value.trim() !== q) return;
      P.searchSupported = r.supported;
      P.searchError = r.error || null;
      P.results = r.results || [];
    } catch (e) {
      P.searchError = String(e);
      P.results = [];
    }
    renderPullPanel();
  }, 300);
}

// The options: "Pull <what was typed>", then the search results.
function pullOptions() {
  const q = P.dialog.querySelector(".pull-input").value.trim();
  const out = [];
  // A result with the very name typed: its details go on the first row.
  const same = P.results.find((r) => r.name === q);
  if (q) out.push({ ...(same || {}), kind: "typed", name: q });
  for (const r of P.results) if (r.name !== q) out.push({ kind: "result", ...r });
  return out;
}

function onPullKey(e) {
  if (P.step !== "search") return;
  const options = pullOptions();
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    if (!options.length) return;
    P.active = (P.active + (e.key === "ArrowDown" ? 1 : -1) + options.length) % options.length;
    renderPullPanel();
  } else if (e.key === "Enter") {
    e.preventDefault();
    const o = options[P.active];
    if (o) chooseOption(o);
  }
}

function chooseOption(o) {
  if (o.kind === "typed") {
    startPull(o.name);
  } else {
    P.chosen = o;
    P.step = "tag";
    renderPullPanel();
  }
}

async function startPull(image) {
  const loc = currentLocation();
  if (loc.needsHost && !loc.host) {
    P.step = "host";
    renderPullPanel();
    return;
  }
  P.step = "pulling";
  P.busy = true;
  P.lines = [t("pull.working")];
  P.pulling = image;
  renderPullPanel();
  pullLine = (line) => {
    P.lines.push(line);
    if (P.lines.length > 200) P.lines.splice(0, P.lines.length - 200);
    const log = P.dialog.querySelector(".pull-log");
    if (log) {
      log.textContent = P.lines.slice(-12).join("\n");
    }
  };
  try {
    const ref = await API.pull(image, loc.id, loc.host);
    P.step = "done";
    P.pulled = ref || image;
    toast(t("done.pull", { name: P.pulled }));
    refreshSoon();
  } catch (e) {
    P.step = "done";
    P.pullError = String(e);
  } finally {
    pullLine = null;
    P.busy = false;
  }
  renderPullPanel();
}

function toggleLocMenu() {
  const menu = P.dialog.querySelector(".loc-menu");
  if (menu.hidden) {
    renderLocMenu();
    menu.hidden = false;
    P.dialog.querySelector(".loc-button").setAttribute("aria-expanded", "true");
    menu.querySelector("input").focus();
  } else {
    closeLocMenu();
  }
}

function closeLocMenu() {
  P.dialog.querySelector(".loc-menu").hidden = true;
  const b = P.dialog.querySelector(".loc-button");
  b.setAttribute("aria-expanded", "false");
  b.focus();
}

const registryAliases = {
  alibaba: "Alibaba ACR Aliyun 알리바바 클라우드 阿里云 阿里雲 アリババクラウド",
  acr: "Microsoft Azure ACR 애저 에저",
};

function renderLocMenu() {
  const menu = P.dialog.querySelector(".loc-menu");
  const search = h("input", { type: "search", class: "loc-search", placeholder: t("search.placeholder"),
    "aria-label": t("pull.location"), autocomplete: "off", spellcheck: "false",
    oninput: () => {
      const terms = search.value.trim().toLowerCase().split(/\s+/);
      for (const row of menu.querySelectorAll(".loc-row")) {
        row.hidden = !terms.every((term) => row.dataset.search.includes(term)) && row.dataset.id !== "custom";
      }
    },
    onkeydown: (event) => {
      if (event.key === "ArrowDown" || event.key === "Enter") {
        event.preventDefault();
        const first = menu.querySelector(".loc-row:not([hidden]) .loc-choice");
        if (event.key === "Enter") first?.click(); else first?.focus();
      }
    },
  });
  put(menu,
    h("div", { class: "loc-filter" }, h("p", { class: "loc-title", text: t("pull.location") }), search),
    h("ul", {}, P.locations.map((l) => {
      const chosen = l.id === P.location;
      const label = l.needsHost && l.host ? `${l.label} (${l.host})` : l.id === "custom" ? t("pull.myRegistry") : l.label;
      return h("li", { class: "loc-row", "data-id": l.id, "data-search": [label, l.id, l.host, l.hostExample, registryAliases[l.id]].join(" ").toLowerCase() },
        h("button", {
          type: "button", class: "loc-choice", title: label, "aria-pressed": chosen ? "true" : "false",
          onclick: () => {
            P.location = l.id;
            P.results = [];
            closeLocMenu();
            if (l.needsHost) {
              P.afterHost = "search";
              P.step = "host";
              renderPullPanel();
            } else {
              onPullInput();
            }
          },
        },
        registryIcon(l.id),
        h("span", { class: "loc-details" },
          h("span", { class: "loc-label", text: label }),
          l.account ? h("span", { class: "loc-account" }, h("span", { class: "dot dot-on", "aria-hidden": "true" }), l.account) : null),
        chosen ? icon("check") : null),
        l.account ? null : h("button", {
          type: "button", class: "button small", text: t("pull.login"),
          "aria-label": `${t("pull.login")}: ${label}`,
          onclick: () => {
            P.location = l.id;
            closeLocMenu();
            P.step = l.needsHost && !l.host ? "host" : "login";
            P.afterHost = "login";
            renderPullPanel();
          },
        }));
    })));
}

function renderPullPanel() {
  if (!P.dialog) return;
  const loc = currentLocation();
  const input = P.dialog.querySelector(".pull-input");
  const searching = loc.id === "dockerhub" && P.searchSupported;
  input.placeholder = searching ? t("pull.searchPlaceholder") : t("pull.refPlaceholder", { name: loc.label });
  input.setAttribute("aria-label", input.placeholder);
  input.disabled = P.busy;
  const lb = P.dialog.querySelector(".loc-button");
  put(lb, registryIcon(loc.id), loc.account ? h("span", { class: "dot dot-on", "aria-hidden": "true" }) : null,
    h("span", { text: loc.id === "custom" && !loc.host ? t("pull.myRegistry") : loc.label }), icon("forward"));
  lb.setAttribute("aria-label", `${t("pull.location")}: ${loc.label}`);
  lb.disabled = P.busy;
  P.dialog.setAttribute("aria-label", t("pull.title"));
  const body = P.dialog.querySelector(".pull-body");
  input.setAttribute("aria-expanded", "false");
  input.removeAttribute("aria-activedescendant");
  if (P.step === "search") put(body, searchBody(input));
  else if (P.step === "tag") put(body, tagBody());
  else if (P.step === "host") put(body, hostBody(loc));
  else if (P.step === "login") put(body, loginBody(loc));
  else put(body, progressBody());
  const kbd = (k) => h("kbd", { text: k });
  put(P.dialog.querySelector(".pull-foot"),
    h("span", {}, kbd("↑"), kbd("↓"), " ", t("pull.keyMove"), "  ", kbd("Enter"), " ", t("pull.keyPull"), "  ", kbd("Esc"), " ", t("pull.keyClose")),
    h("span", {}, kbd(IS_MAC ? "⌘⇧P" : "Ctrl+Shift+P"), " ", t("pull.keyAnywhere")));
  const focus = body.querySelector("[data-focus]");
  if (focus) focus.focus();
}

function searchBody(input) {
  const options = pullOptions();
  const notes = [];
  if (!P.searchSupported && currentLocation().id === "dockerhub") notes.push(h("p", { class: "pull-note", text: t("pull.noSearch") }));
  if (P.searchError) notes.push(h("p", { class: "pull-note error-text", text: P.searchError }));
  if (!options.length) {
    return [h("p", { class: "pull-note", text: currentLocation().id === "dockerhub" && P.searchSupported ? t("pull.hint") : t("pull.hintRef") }), notes];
  }
  if (P.active >= options.length) P.active = 0;
  input.setAttribute("aria-expanded", "true");
  input.setAttribute("aria-activedescendant", `pull-option-${P.active}`);
  const list = h("ul", { class: "pull-results", id: "pull-results", role: "listbox", "aria-label": t("pull.title") },
    options.map((o, i) => h("li", {
      id: `pull-option-${i}`, role: "option", class: "pull-option", "aria-selected": i === P.active ? "true" : "false",
      onclick: () => chooseOption(o), onmousemove: () => { if (P.active !== i) { P.active = i; markActive(); } },
    }, icon(o.kind === "typed" ? "pull" : "images"),
    h("span", { class: "pull-main" },
      h("span", { class: "pull-name" },
        h("span", { text: o.kind === "typed" ? t("pull.pullThis", { name: o.name }) : o.name }),
        o.is_official ? h("span", { class: "tag official", text: t("pull.official") }) : null),
      o.description ? h("span", { class: "pull-desc", text: o.description }) : null),
    o.star_count != null ? h("span", { class: "pull-stars", title: t("pull.stars") }, icon("star"), String(o.star_count)) : null)));
  return [notes, list];
}

function markActive() {
  P.dialog.querySelectorAll(".pull-option").forEach((el, i) => el.setAttribute("aria-selected", i === P.active ? "true" : "false"));
  P.dialog.querySelector(".pull-input").setAttribute("aria-activedescendant", `pull-option-${P.active}`);
}

function tagBody() {
  const name = P.chosen.name;
  const tag = h("input", { type: "text", id: "pull-tag", value: "latest", "data-focus": "", autocomplete: "off", spellcheck: "false" });
  tag.value = "latest";
  const form = h("form", { class: "pull-form", onsubmit: (e) => { e.preventDefault(); startPull(`${name}:${tag.value.trim() || "latest"}`); } },
    h("label", { for: "pull-tag", text: t("pull.tagFor", { name }) }), tag,
    h("div", { class: "buttons" }, h("button", { type: "submit", class: "button primary", text: t("action.pull") })));
  setTimeout(() => tag.select(), 0);
  return form;
}

function hostBody(loc) {
  const host = h("input", { type: "text", id: "pull-host", value: loc.host, "data-focus": "", autocomplete: "off", spellcheck: "false", placeholder: loc.hostExample || "registry.example.com" });
  return h("form", { class: "pull-form", onsubmit: async (e) => {
    e.preventDefault();
    const v = host.value.trim();
    if (!v) return;
    P.hosts[loc.id] = v;
    saveHosts();
    await loadLocations();
    P.step = P.afterHost === "login" ? "login" : "search";
    P.afterHost = null;
    renderPullPanel();
    if (P.step === "search") P.dialog.querySelector(".pull-input").focus();
  } },
  h("label", { for: "pull-host", text: t("pull.host", { name: loc.label }) }), host,
  h("div", { class: "buttons" }, h("button", { type: "submit", class: "button primary", text: t("action.save") })));
}

function loginBody(loc) {
  if (loc.hint) {
    return h("div", { class: "pull-form" },
      h("p", { text: t("pull.loginCloud", { name: loc.label }) }),
      codeLine(loc.hint));
  }
  const user = h("input", { type: "text", id: "login-user", "data-focus": "", autocomplete: "username", spellcheck: "false" });
  const pass = h("input", { type: "password", id: "login-pass", autocomplete: "current-password" });
  const status = h("p", { class: "progress", "aria-live": "polite" });
  const submit = h("button", { type: "submit", class: "button primary", text: t("pull.login") });
  return h("form", { class: "pull-form", onsubmit: async (e) => {
    e.preventDefault();
    if (!user.value.trim() || !pass.value) return;
    submit.disabled = true;
    status.textContent = t("pull.loggingIn");
    const secret = pass.value;
    pass.value = "";
    try {
      await API.registryLogin(loc.id, loc.host || "", user.value.trim(), secret);
      toast(t("pull.loggedIn", { name: loc.label }));
      await loadLocations();
      P.step = "search";
      renderPullPanel();
      P.dialog.querySelector(".pull-input").focus();
    } catch (err) {
      status.textContent = String(err);
      submit.disabled = false;
    }
  } },
  h("p", { class: "pull-form-title", text: t("pull.loginTo", { name: loc.label }) }),
  h("label", { for: "login-user", text: t("pull.user") }), user,
  h("label", { for: "login-pass", text: t("pull.password") }), pass,
  h("p", { class: "hint", text: t("pull.passwordNote") }),
  status,
  h("div", { class: "buttons" }, submit));
}

function progressBody() {
  if (P.step === "pulling") {
    return h("div", { class: "pull-form" },
      h("p", { class: "pull-form-title", text: t("pull.pulling", { name: P.pulling }) }),
      h("pre", { class: "pull-log", "aria-live": "polite", text: P.lines.slice(-12).join("\n") }));
  }
  const ok = !P.pullError;
  const out = h("div", { class: "pull-form" },
    h("p", { class: ok ? "pull-form-title" : "error-text", text: ok ? t("done.pull", { name: P.pulled }) : P.pullError }),
    h("div", { class: "buttons" },
      h("button", { type: "button", class: "button", text: t("pull.another"), onclick: () => { P.step = "search"; P.pullError = null; P.dialog.querySelector(".pull-input").value = ""; renderPullPanel(); P.dialog.querySelector(".pull-input").focus(); } }),
      h("button", { type: "button", class: "button primary", "data-focus": "", text: t("pull.close"), onclick: () => P.dialog.close() })));
  P.pullError = ok ? null : P.pullError;
  return out;
}

// ---- the docker command ----

async function refreshDocker() {
  try {
    S.docker = await API.dockerCommand();
  } catch {
    S.docker = null;
  }
  renderBanner();
  renderStatusbar();
  const box = document.getElementById("docker-settings");
  if (box) box.replaceWith(dockerSettings());
}

async function switchDocker(to) {
  try {
    S.docker = await API.dockerSwitch(to);
    toast(to === "use" ? t("docker.nowPodbag") : t("docker.nowOther", { name: S.docker.name }));
  } catch (e) {
    toast(String(e), true);
  }
  await refreshDocker();
}

// The status bar's "docker → …": a click offers the switch, after a question.
dockerCommandItem = function () {
  const d = S.docker;
  if (!d) return null;
  if (!d.installed) return h("span", { class: "status-item", text: t("docker.missingShort") });
  const text = `docker → ${d.name}`;
  return h("button", {
    type: "button", class: `status-item ${d.isPodbag ? "" : "status-warn"}`, title: t("docker.status", { name: d.name }),
    onclick: async () => {
      if (!d.isPodbag) {
        if (await confirmDialog(t("docker.askUse", { name: d.name }), t("docker.use"))) switchDocker("use");
      } else if (d.previous) {
        if (await confirmDialog(t("docker.askBack", { name: d.previous }), t("docker.back", { name: d.previous }))) switchDocker("back");
      } else {
        go("settings");
      }
    },
  }, d.isPodbag ? null : icon("warning"), text);
};

dockerSettings = function () {
  const d = S.docker;
  const box = h("div", { class: "docker-settings", id: "docker-settings" });
  if (!d) return box;
  put(box,
    h("dl", { class: "fields" }, field(t("docker.command"), d.installed ? d.name : t("docker.missing"))),
    d.installed && d.fromEnv ? h("p", { class: "hint warn", text: t("docker.env") }) : null,
    d.installed ? h("div", { class: "buttons-row" },
      d.isPodbag ? null : button(null, t("docker.use"), () => switchDocker("use"), "primary"),
      d.isPodbag && d.previous ? button(null, t("docker.back", { name: d.previous }), () => switchDocker("back")) : null) : null,
    d.installed ? h("p", { class: "hint", text: t("docker.note") }) : null);
  return box;
};

let bannerClosed = false;
renderBanner = function () {
  const el = document.getElementById("banner");
  if (!el) return;
  const d = S.docker;
  const show = !bannerClosed && d && d.installed && !d.isPodbag && isPodbag() && S.engine && S.engine.reachable;
  el.hidden = !show;
  if (!show) return;
  el.setAttribute("aria-label", t("docker.command"));
  put(el,
    icon("warning"),
    h("span", { class: "banner-text" },
      h("strong", { text: t("docker.banner", { name: d.name }) }),
      d.fromEnv ? ` ${t("docker.env")}` : null),
    button(null, t("docker.use"), () => switchDocker("use"), "primary"),
    iconButton("close", t("pull.close"), () => { bannerClosed = true; renderBanner(); }));
};

// ---- import from Docker ----

async function importFromDocker() {
  let list;
  try {
    list = await API.dockerImages(Object.entries(P.hosts));
  } catch (e) {
    toast(String(e), true);
    return;
  }
  const have = new Set(S.lists.images.flatMap((i) => i.RepoTags || []));
  const boxes = list.map(({ name, size: bytes, registry }) => {
    return h("label", { class: "check import-row" },
      h("input", { type: "checkbox", value: name }),
      registryIcon(registry),
      h("span", { class: "truncate", text: name, title: name }),
      h("span", { class: "secondary-text", text: have.has(name) ? t("import.have") : size(bytes) }));
  });
  const status = h("p", { class: "progress", "aria-live": "polite" });
  const d = h("dialog", { class: "import", "aria-labelledby": "import-title" },
    h("form", { method: "dialog" },
      h("h2", { id: "import-title" }, registryIcon("dockerhub"), t("import.title")),
      h("p", { text: t("import.text") }),
      list.length ? h("div", { class: "import-list" }, boxes) : h("p", { text: t("import.none") }),
      status,
      h("div", { class: "buttons" },
        h("button", { value: "", class: "button", text: t("action.cancel") }),
        h("button", { value: "copy", class: "button primary", text: t("import.copy"), disabled: !list.length }))));
  d.addEventListener("submit", async (e) => {
    if (!e.submitter || e.submitter.value !== "copy") return;
    e.preventDefault();
    const names = [...d.querySelectorAll("input:checked")].map((c) => c.value);
    if (!names.length) return;
    d.querySelectorAll("button, input").forEach((b) => { b.disabled = true; });
    status.textContent = t("import.working", { n: names.length });
    try {
      await API.importImages(names);
      toast(t("import.done", { n: names.length }));
      refreshSoon();
      d.close();
    } catch (err) {
      status.textContent = String(err);
      d.querySelectorAll("button, input").forEach((b) => { b.disabled = false; });
    }
  });
  d.addEventListener("close", () => d.remove());
  document.body.append(d);
  d.showModal();
}

// ---- hooks into app.js ----

pull = openPullPanel;
extraKeys = function (e) {
  const shift = e.shiftKey && (IS_MAC ? e.metaKey : e.ctrlKey);
  if (shift && e.key.toLowerCase() === "p" && !document.querySelector("dialog[open]")) {
    e.preventDefault();
    openPullPanel();
  } else if (!IS_MAC && e.ctrlKey && e.key === ",") {
    e.preventDefault();
    go("settings");
  }
};

window.podbag.pull = openPullPanel;
window.podbag.importFromDocker = importFromDocker;
window.podbag.pullState = P;
window.podbag.render = renderPullPanel;
window.podbag.searchFor = (q) => {
  const input = P.dialog.querySelector(".pull-input");
  input.value = q;
  onPullInput();
};
init();
