// A manifest contributes data to fixed host controls. Its content is always text.
"use strict";

const Extensions = { items: [], selected: null, loading: false, error: null };

function openExtensions(main) {
  renderExtensions(main);
  extensionAction({ action: "list" });
}

function updateExtensions() {
  if (S.view === "extensions") renderExtensions(document.getElementById("main"));
}

async function extensionAction(request) {
  if (Extensions.loading) return;
  Extensions.loading = true;
  Extensions.error = null;
  updateExtensions();
  try {
    const previous = Extensions.items;
    const installed = await API.extensions(request);
    if (request.action === "install") {
      const newItem = installed.find((item) => !previous.some((old) => old.manifest.id === item.manifest.id));
      if (newItem) Extensions.selected = newItem.manifest.id;
    }
    Extensions.items = installed;
  } catch (error) {
    if (["large", "manifest", "version", "duplicate", "full", "notFound", "permission", "storage"].includes(error?.kind)) {
      Extensions.error = error;
    } else {
      Extensions.error = { kind: "storage" };
    }
  } finally {
    Extensions.loading = false;
    updateExtensions();
  }
}

async function installExtension(file) {
  if (!file || Extensions.loading) return;
  if (file.size > 128 * 1024) {
    Extensions.error = { kind: "large" };
    updateExtensions();
    return;
  }
  let source;
  try {
    source = new TextDecoder("utf-8", { fatal: true }).decode(await file.arrayBuffer());
  } catch {
    Extensions.error = { kind: "manifest" };
    updateExtensions();
    return;
  }
  await extensionAction({ action: "install", source });
}

function renderExtensions(main) {
  const selected = Extensions.items.find((item) => item.manifest.id === Extensions.selected) || Extensions.items[0];
  Extensions.selected = selected?.manifest.id ?? null;
  const file = h("input", { type: "file", accept: ".json,application/json", hidden: true, onchange: () => installExtension(file.files[0]) });
  const install = button("import", t("extensions.install"), () => file.click(), "primary");
  install.disabled = Extensions.loading;
  const error = Extensions.error;
  put(main,
    h("header", { class: "toolbar" },
      h("h1", { text: t("nav.extensions") }), h("div", { class: "tools" }, file, install)),
    h("div", { class: "content extensions" },
      error ? h("div", { role: "alert", class: "extension-error" },
        h("p", { class: "error-text", text: t(`extensions.error.${error.kind}`) }),
        error.detail ? h("p", { class: "hint", text: error.detail }) : null) : null,
      Extensions.loading ? h("p", { class: "hint", role: "status", text: t("extensions.loading") }) : null,
      selected ? h("div", { class: "extension-grid" },
        extensionList(selected), extensionDetails(selected)) :
        h("p", { class: "hint", text: t("extensions.empty") })));
  refreshExtensionSummary(main);
}

function extensionList(selected) {
  return h("section", { class: "card extension-list" },
    h("h2", { class: "panel-title", text: t("extensions.installed") }),
    h("ul", {}, Extensions.items.map((entry) => {
      const { id, name } = entry.manifest;
      return h("li", {}, h("button", {
        type: "button", "aria-current": id === selected.manifest.id ? "true" : null,
        onclick: () => { Extensions.selected = id; updateExtensions(); },
      }, h("span", { text: name }),
      h("small", { text: t(entry.enabled ? "extensions.enabled" : "extensions.disabled") })));
    })));
}

function extensionDetails(entry) {
  const { id, name, description, version, permissions, content } = entry.manifest;
  const enable = button("power", t(entry.enabled ? "extensions.disable" : "extensions.enable"),
    () => extensionAction({ action: "setEnabled", id, enabled: !entry.enabled }), entry.enabled ? "" : "primary");
  const remove = button("remove", t("action.remove"), async () => {
    if (await confirmDialog(t("extensions.confirmRemove", { name }), t("action.remove"))) {
      extensionAction({ action: "remove", id });
    }
  });
  enable.disabled = remove.disabled = Extensions.loading;
  const grant = h("input", { type: "checkbox", checked: entry.engineSummary, disabled: Extensions.loading || !entry.enabled,
    onchange: (event) => extensionAction({ action: "setEngineSummary", id, allow: event.target.checked }) });
  return h("div", { class: "extension-details" },
    h("section", { class: "card extension-card" },
      h("h2", { text: name }), h("p", { class: "secondary-text", text: `${id} · ${version}` }),
      h("p", { class: "extension-description", text: description }),
      h("div", { class: "buttons-row" }, enable, remove),
      permissions.includes("engineSummary") ? h("label", { class: "check extension-permission" }, grant, t("extensions.permission")) : null),
    entry.enabled ? h("section", { class: "card extension-card extension-content" },
      h("h2", { text: t("extensions.view") }), content.map((block) => extensionBlock(block, entry.engineSummary))) : null);
}

function extensionBlock(block, allowed) {
  switch (block.type) {
    case "heading": return h("h3", { text: block.text });
    case "text": return h("p", { text: block.text });
    case "list": return h("ul", {}, block.items.map((item) => h("li", { text: item })));
    case "code": return h("pre", {}, h("code", { text: block.text }));
    case "engineSummary": return allowed
      ? h("section", { class: "extension-engine-summary" }, h("h3", { text: t("extensions.engineSummary") }), h("div", { id: "extension-counts" }))
      : h("p", { class: "hint", text: t("extensions.noGrant") });
  }
}

function refreshExtensionSummary(main) {
  const summary = main.querySelector("#extension-counts");
  if (!summary) return;
  if (!S.loaded || !S.engine?.reachable) {
    put(summary, h("p", { class: "hint", text: t("extensions.engineOffline") }));
    return;
  }
  put(summary, h("dl", {}, KINDS.map((kind) => h("div", {},
    h("dt", { text: t(`nav.${kind}`) }),
    h("dd", { text: S.listErrors[kind] ? t("detail.none") : S.lists[kind].length })))));
}
