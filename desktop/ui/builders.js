"use strict";

function builderSettings() {
  const context = h("p", { class: "hint" });
  const status = h("p", { class: "builder-status", role: "status" });
  const list = h("ul", { class: "builder-list", "aria-label": t("settings.builders") });
  const detail = h("div", { class: "builder-inspection" });
  const refresh = button("refresh", t("action.refresh"), () => load());
  const root = h("div", { class: "builder-settings" },
    h("div", { class: "builder-toolbar" }, context, refresh), status, list, detail);
  const section = settingDetails("builder-settings", "builders", "engine", null, root);
  let catalog, open, inspection, busy = false;

  section.addEventListener("toggle", () => {
    if (section.open && !catalog && !busy) load();
  });

  async function work(task, label) {
    if (busy || !root.isConnected) return;
    busy = true;
    status.classList.remove("error-text");
    status.textContent = label;
    root.setAttribute("aria-busy", "true");
    render();
    try {
      await task();
      if (root.isConnected) status.textContent = "";
    } catch (error) {
      if (root.isConnected) {
        status.classList.add("error-text");
        status.textContent = String(error);
      }
    } finally {
      busy = false;
      if (root.isConnected) {
        root.removeAttribute("aria-busy");
        render();
      }
    }
  }

  function load() {
    return work(loadCatalog, t("builders.loading"));
  }

  async function loadCatalog() {
    catalog = await API.builderList();
    if (open && !catalog.builders.some((entry) => entry.name === open)) open = null;
    inspection = open ? await API.builderInspect(catalog.scope, open) : null;
  }

  async function show(name) {
    if (open === name) {
      open = null;
      inspection = null;
      render();
      list.querySelector(`[data-builder="${CSS.escape(name)}"]`)?.focus();
      return;
    }
    open = name;
    inspection = null;
    await work(async () => { inspection = await API.builderInspect(catalog.scope, name); }, t("builders.loading"));
    list.querySelector(`[data-builder="${CSS.escape(name)}"]`)?.focus();
  }

  async function act(name, action) {
    if (busy) return;
    if (action === "remove" && !await confirmDialog(t("builders.removeConfirm", { name }))) return;
    await work(async () => {
      await API.builderAction(catalog.scope, name, action);
      if (action === "remove") {
        open = null;
        inspection = null;
      }
      await loadCatalog();
    }, t("builders.working"));
    const row = list.querySelector(`[data-builder="${CSS.escape(name)}"]`);
    if (root.isConnected) (row || refresh).focus();
  }

  function render() {
    refresh.disabled = busy;
    const ready = catalog?.availability === "ready";
    context.textContent = ready ? t("builders.context", { context: catalog.context }) : "";
    list.hidden = !ready || catalog.builders.length === 0;
    detail.hidden = true;
    put(list, []);
    put(detail, []);
    if (!ready) {
      if (!busy && catalog) status.textContent = t(`builders.${catalog.availability}`);
      return;
    }
    if (!busy && !status.textContent) {
      status.textContent = catalog.outsideScope ? t("builders.outsideScope")
        : catalog.builders.length ? "" : t("builders.empty");
    }
    put(list, catalog.builders.map((entry) =>
      h("li", {}, h("button", {
        type: "button", class: "builder-row", "data-builder": entry.name,
        "aria-expanded": open === entry.name ? "true" : "false", disabled: busy,
        onclick: () => show(entry.name),
      },
      h("span", { class: "builder-name", text: entry.name }),
      h("span", { class: "builder-driver mono", text: entry.driver }),
      entry.current ? h("span", { class: "builder-current", text: t("builders.current") }) : null,
      icon("forward")))));
    if (open && inspection) {
      const entry = catalog.builders.find((builder) => builder.name === open);
      if (entry) renderInspection(entry);
    }
  }

  function renderInspection(entry) {
    detail.hidden = false;
    const running = inspection.nodes.some((node) => node.status === "running");
    const actions = [button("check", t("builders.select"), () => act(entry.name, "select"))];
    actions[0].disabled = entry.current;
    if (entry.managed) {
      const action = running ? "stop" : "start";
      actions.push(button(action, t(`action.${action}`), () => act(entry.name, action)),
        button("remove", t("action.remove"), () => act(entry.name, "remove"), "builder-remove"));
    }
    if (busy) for (const action of actions) action.disabled = true;
    put(detail,
      h("div", { class: "builder-toolbar" }, h("h3", { text: entry.name }), h("div", { class: "builder-actions" }, actions)),
      inspection.error ? h("p", { class: "error-text", text: inspection.error }) : null,
      inspection.nodes.map((node) => h("section", { class: "builder-node" },
        h("h4", { text: node.name }),
        h("dl", { class: "fields" },
          field(t("builders.endpoint"), node.endpoint, "mono"),
          field(t("detail.status"), builderStatus(node.status)),
          node.buildkit ? field(t("builders.version"), node.buildkit, "mono") : null,
          node.platforms ? field(t("builders.platforms"), node.platforms) : null),
        node.error ? h("p", { class: "error-text", text: node.error }) : null)));
  }

  return section;
}

function builderStatus(status) {
  return ["running", "stopped", "inactive", "starting", "stopping"].includes(status)
    ? t(`builders.status.${status}`) : status || t("detail.none");
}
