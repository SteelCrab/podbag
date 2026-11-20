// The Kubernetes view reads only the selected resource and namespace scope.
"use strict";

const KUBE_KINDS = ["pods", "services"];

function kubeColumns(kind) {
  return kind === "pods"
    ? ["col.name", "kube.namespace", "kube.phase", "kube.ready", "kube.node"]
    : ["col.name", "kube.namespace", "kube.type", "kube.address", "col.ports"];
}

function kubeCells(kind, item) {
  const { metadata = {}, spec = {}, status = {} } = item;
  const identity = [metadata.name, metadata.namespace];
  if (kind === "pods") {
    const sidecars = (spec.initContainers || []).filter((c) => c.restartPolicy === "Always");
    const containers = [...(spec.containers || []), ...sidecars];
    const ready = new Set([...(status.containerStatuses || []), ...(status.initContainerStatuses || [])]
      .filter((c) => c.ready).map((c) => c.name));
    const count = containers.filter((c) => ready.has(c.name)).length;
    return [...identity, status.phase, `${count}/${containers.length}`, spec.nodeName];
  }
  const ports = (spec.ports || []).map((p) =>
    `${p.port}${p.nodePort ? ":" + p.nodePort : ""}/${p.protocol || "TCP"}`);
  const address = spec.externalName || (spec.clusterIPs || []).join(", ") || spec.clusterIP;
  return [...identity, spec.type, address, ports.join(", ")];
}

function inspectKubeObject(item, context) {
  const title = `${item.metadata.namespace} / ${item.metadata.name}`;
  const dialog = h("dialog", { class: "kube-inspect", "aria-labelledby": "kube-object-title" },
    h("form", { method: "dialog" },
      h("h2", { id: "kube-object-title", text: title }),
      h("p", { text: t("kube.context", { context }) }),
      h("pre", { class: "json", tabindex: "0", text: JSON.stringify(item, null, 2) }),
      h("div", { class: "buttons" }, h("button", { class: "button", autofocus: true, text: t("action.back") }))));
  document.body.append(dialog);
  dialog.addEventListener("close", () => dialog.remove(), { once: true });
  dialog.showModal();
}

function renderKubernetes(main) {
  let items = [];
  let context = "";
  let generation = 0;
  const scope = h("p", { class: "kube-scope secondary-text" });
  const content = h("div", { class: "content", "aria-live": "polite" });
  const search = h("input", { type: "search", class: "search", placeholder: t("search.placeholder"),
    "aria-label": t("search.placeholder"), oninput: renderRows });
  const all = h("input", { type: "checkbox", onchange: load });
  const tabs = KUBE_KINDS.map((kind) => h("button", {
    type: "button", class: "tab", id: `kube-${kind}`, "aria-pressed": String(kind === S.kubeKind),
    onclick: () => go("kubernetes", kind), text: t(`kube.${kind}`),
  }));
  main.replaceChildren(
    h("header", { class: "toolbar" }, h("h1", { text: t("nav.kubernetes") }),
      h("label", { class: "search-box" }, icon("search"), search),
      h("div", { class: "tools" }, windowBar ? null : iconButton("refresh", t("action.refresh"), load))),
    h("div", { class: "tabs kube-tabs", role: "group", "aria-label": t("nav.kubernetes") }, tabs,
      h("label", { class: "check" }, all, t("kube.allNamespaces"))),
    scope, content);

  function renderRows() {
    const kind = S.kubeKind;
    const q = search.value.trim().toLowerCase();
    const rows = [];
    for (const item of items) {
      const values = kubeCells(kind, item);
      if (q && !values.some((v) => String(v || "").toLowerCase().includes(q))) continue;
      const open = () => inspectKubeObject(item, context);
      const name = h("button", { type: "button", class: "link primary-text", text: values[0], title: values[0], onclick: open });
      rows.push(h("tr", { onclick: (e) => { if (!e.target.closest("button")) name.click(); } },
        h("td", {}, name), values.slice(1).map((v) => h("td", {}, h("span", { text: v || "—", title: v })))));
    }
    if (!rows.length) {
      const text = items.length ? t("empty.search", { q: search.value.trim() }) : t(`kube.empty.${kind}`);
      content.replaceChildren(h("p", { class: "nothing", text }));
      return;
    }
    content.replaceChildren(h("table", { class: "list kube-list", "aria-label": t(`kube.${kind}`) },
      h("thead", {}, h("tr", {}, kubeColumns(kind).map((key) => h("th", { scope: "col", text: t(key) })))),
      h("tbody", {}, rows)));
  }

  async function load() {
    const request = ++generation;
    search.disabled = true;
    items = [];
    scope.textContent = "";
    content.replaceChildren(h("p", { class: "loading", text: t("engine.checking") }));
    try {
      const result = await API.kubeList(S.kubeKind, all.checked);
      if (request !== generation || !content.isConnected) return;
      context = result.context;
      scope.textContent = [t("kube.context", { context }), all.checked ? t("kube.allNamespaces") : result.namespace].join(" · ");
      items = result.items;
      search.disabled = false;
      renderRows();
    } catch (error) {
      if (request !== generation || !content.isConnected) return;
      const message = String(error) === "kubectl-missing" ? t("kube.missing") : String(error);
      content.replaceChildren(h("div", { class: "empty" },
        h("h2", { text: t("kube.error") }), h("p", { class: "error-text mono", text: message }),
        button("refresh", t("action.retry"), load)));
    }
  }

  load();
  return {
    reload: load,
    switchResource() {
      tabs.forEach((tab, i) => tab.setAttribute("aria-pressed", String(KUBE_KINDS[i] === S.kubeKind)));
      search.value = "";
      load();
    },
  };
}
