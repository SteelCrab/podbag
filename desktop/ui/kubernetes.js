// The Kubernetes view reads only the selected resource and namespace scope.
"use strict";

const KUBE_COLUMNS = {
  pods: ["kube.phase", "kube.ready", "kube.node"],
  services: ["kube.type", "kube.address", "col.ports"],
  deployments: ["kube.ready", "col.image"],
  claims: ["kube.phase", "kube.capacity", "kube.storageClass"],
};
const KUBE_KINDS = Object.keys(KUBE_COLUMNS);

function kubeColumns(kind) {
  return ["col.name", "kube.namespace", ...KUBE_COLUMNS[kind]];
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
  if (kind === "deployments") {
    const images = (spec.template?.spec?.containers || []).map((c) => c.image);
    return [...identity, `${status.readyReplicas ?? 0}/${spec.replicas ?? 1}`, images.join(", ")];
  }
  if (kind === "claims") {
    return [...identity, status.phase, status.capacity?.storage, spec.storageClassName];
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

function openLocalCluster(onChange) {
  const section = h("div", { "aria-live": "polite" });
  const message = h("p", { class: "local-kube-message secondary-text", "aria-live": "polite" });
  const close = h("button", { type: "button", class: "button", text: t("action.back") });
  const dialog = h("dialog", { class: "local-kube", "aria-labelledby": "local-kube-title" },
    h("h2", { id: "local-kube-title", text: t("kube.localTitle") }),
    h("p", { text: t("kube.localPrerequisites") }),
    section,
    message,
    h("div", { class: "buttons" }, close));
  let busy = false;
  close.addEventListener("click", () => dialog.close());
  dialog.addEventListener("close", () => dialog.remove(), { once: true });
  dialog.addEventListener("cancel", (event) => {
    if (busy) event.preventDefault();
  });

  function show(cluster) {
    if (cluster) {
      const reset = button(null, t("kube.localReset"), async () => {
        if (await confirmDialog(t("kube.localResetConfirm"), t("kube.localReset"))) {
          await perform(t("kube.localResetting"), API.localKubeReset);
        }
      });
      const remove = button(null, t("action.remove"), async () => {
        if (await confirmDialog(t("kube.localRemoveConfirm"), t("action.remove"))) {
          await perform(t("kube.localRemoving"), async () => {
            await API.localKubeRemove();
            return null;
          }, cluster.context);
        }
      });
      section.replaceChildren(h("dl", { class: "fields" },
        field(t("kube.cluster"), cluster.context, "mono"),
        field(t("kube.localVersion"), cluster.version || t("kube.localDefault")),
        field(t("kube.localNodes"), cluster.nodes),
        field(t("settings.vmState"), t(cluster.ready ? "kube.localProvisioned" : "kube.localIncomplete"))),
        h("div", { class: "buttons" }, reset, remove));
      return;
    }
    const version = h("input", {
      id: "local-kube-version", name: "version", type: "text", placeholder: t("kube.localDefault"),
      pattern: "v1[.][0-9]{1,3}[.][0-9]{1,3}", title: "v1.35.0",
    });
    const nodes = h("input", { id: "local-kube-nodes", name: "nodes", type: "number", min: "1", max: "255", step: "1", value: "1", required: true });
    const form = h("form", { class: "local-kube-form" },
      h("label", { for: "local-kube-version" }, t("kube.localVersion"), version),
      h("label", { for: "local-kube-nodes" }, t("kube.localNodes"), nodes),
      h("button", { type: "submit", class: "button primary", text: t("kube.localCreate") }));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      perform(t("kube.localCreating"), () => API.localKubeCreate(version.value.trim(), nodes.valueAsNumber));
    });
    section.replaceChildren(form);
    if (dialog.open && !busy) version.focus();
  }

  async function perform(label, action, removedContext) {
    if (busy) return;
    busy = true;
    close.disabled = true;
    section.querySelectorAll("button, input").forEach((el) => { el.disabled = true; });
    message.classList.remove("error-text");
    message.textContent = label;
    try {
      const cluster = await action();
      show(cluster);
      message.textContent = "";
      onChange(cluster, removedContext);
    } catch (error) {
      message.classList.add("error-text");
      message.textContent = String(error);
      await API.localKubeStatus().then(show).catch(() => {});
    } finally {
      busy = false;
      close.disabled = false;
      section.querySelectorAll("button, input").forEach((el) => { el.disabled = false; });
    }
  }

  document.body.append(dialog);
  dialog.showModal();
  API.localKubeStatus().then(show).catch((error) => {
    message.classList.add("error-text");
    message.textContent = String(error);
  });
}

function renderKubernetes(main) {
  let items = [];
  let context = "";
  let generation = 0;
  const scope = h("p", { class: "kube-scope secondary-text" });
  const content = h("div", { class: "content", "aria-live": "polite" });
  const search = h("input", { type: "search", class: "search", placeholder: t("search.placeholder"),
    "aria-label": t("search.placeholder"), oninput: renderRows });
  const all = h("input", { type: "checkbox", checked: S.kubeAllNamespaces, onchange: () => { S.kubeAllNamespaces = all.checked; refreshKubeCounts(); load(); } });
  const cluster = h("select", { id: "kube-context", onchange: () => {
    S.kubeContext = cluster.value;
    sessionStorage.setItem("kube-context", cluster.value);
    refreshKubeCounts();
    load();
  } });
  function contexts(names) {
    const options = [...new Set([...names, S.kubeContext].filter(Boolean))];
    cluster.replaceChildren(h("option", { value: "", text: t("kube.current") }),
      ...options.map((name) => h("option", { value: name, text: name })));
    cluster.value = S.kubeContext;
  }
  contexts([]);
  function reload() {
    refreshKubeCounts();
    API.kubeContexts().then(contexts).catch(() => {});
    return load();
  }
  function localChanged(local, removedContext) {
    if (local) S.kubeContext = local.context;
    else if (S.kubeContext === removedContext) S.kubeContext = "";
    sessionStorage.setItem("kube-context", S.kubeContext);
    reload();
  }
  const tabs = KUBE_KINDS.map((kind) => h("button", {
    type: "button", class: "tab", id: `kube-${kind}`, "aria-pressed": String(kind === S.kubeKind),
    onclick: () => go("kubernetes", kind), text: t(`kube.${kind}`),
  }));
  main.replaceChildren(
    h("header", { class: "toolbar" }, h("h1", { text: t("nav.kubernetes") }),
      h("label", { class: "search-box" }, icon("search"), search),
      h("div", { class: "tools" },
        h("button", { id: "local-kube-open", class: "button", type: "button", text: t("kube.localTitle"),
          onclick: () => openLocalCluster(localChanged) }),
        windowBar ? null : iconButton("refresh", t("action.refresh"), reload))),
    h("div", { class: "tabs kube-tabs", role: "group", "aria-label": t("nav.kubernetes") }, tabs,
      h("label", { class: "check" }, all, t("kube.allNamespaces"))),
    h("label", { class: "kube-cluster" }, t("kube.cluster"), cluster), scope, content);

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
      const result = await API.kubeList(S.kubeKind, all.checked, S.kubeContext);
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
        button("refresh", t("action.retry"), reload)));
    }
  }

  reload();
  return {
    reload,
    switchResource() {
      tabs.forEach((tab, i) => tab.setAttribute("aria-pressed", String(KUBE_KINDS[i] === S.kubeKind)));
      search.value = "";
      load();
    },
  };
}
