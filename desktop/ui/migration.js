"use strict";

function migrationSocket() {
  return h("input", { type: "text", hidden: true, placeholder: "unix:///path/to/docker.sock", "aria-label": t("engine.socket") });
}

const migrationScope = { cluster: false, engine: "", context: "", namespace: "" };

function renderMigration(main) {
  const engineKinds = ["containers", "images", "volumes"];
  const opt = ([value, text]) => h("option", { value, text });
  const tab = (id, label, cluster) => h("button", { id, class: "tab", type: "button", text: t(label),
    onclick: () => { if (migrationScope.cluster !== cluster) { migrationScope.cluster = cluster; reload(); } } });
  const engines = tab("migration-engine-tab", "settings.engine", false);
  const clusters = tab("migration-kube-tab", "nav.kubernetes", true);
  const picker = h("select", { id: "migration-source", "aria-label": t("migration.from") });
  const socket = migrationSocket();
  const scope = h("select", { id: "migration-filter", "aria-label": t("kube.namespace"), onchange: () => {
    migrationScope.namespace = scope.value; draw();
  } });
  const resources = h("div", { class: "migration-list" });
  const status = h("div", { role: "status", class: "migration-statusline" });
  const filters = h("div", { class: "migration-filters" }, picker, socket, scope);
  main.replaceChildren(h("header", { class: "toolbar" }, h("h1", { text: t("migration.title") }),
    windowBar ? null : iconButton("refresh", t("action.refresh"), reload)),
    h("div", { class: "tabs", role: "group", "aria-label": t("migration.from") }, engines, clusters),
    h("div", { class: "content migration-content" }, filters, resources, status));

  let revision = 0, items = [], choices = [], problems = [], pending = 0;
  const current = id => resources.isConnected && revision === id;
  const kinds = () => migrationScope.cluster ? KUBE_KINDS : engineKinds;
  const kindLabel = k => t((migrationScope.cluster ? "kube." : "nav.") + k);
  function draw() {
    const visible = items.filter(item => !migrationScope.cluster || !migrationScope.namespace || item.namespace === migrationScope.namespace);
    resources.replaceChildren(...kinds().flatMap(kind => {
      const rows = visible.filter(item => item.kind === kind).sort((a, b) => a.label.localeCompare(b.label) || a.source.localeCompare(b.source));
      if (!rows.length) return [];
      return h("section", { class: "migration-group" }, h("h2", {}, icon(migrationScope.cluster ? "kube-" + kind : kind),
        h("span", { text: kindLabel(kind) }), h("span", { class: "count", text: rows.length })),
        rows.map(item => h("button", { type: "button", class: "migration-row", "data-kind": kind, "data-name": item.name,
          title: t("migration.review"), onclick: () => confirmMigration(item, choices, reload) },
          h("span", { class: "migration-name", text: item.label }), h("span", { class: "migration-origin", text: migrationScope.cluster ? (migrationScope.namespace ? "" : item.namespace) : item.sourceLabel }), icon("forward"))));
    }));
    const namespaces = [...new Set(items.map(i => i.namespace).filter(Boolean))].sort();
    scope.replaceChildren(opt(["", t("kube.allNamespaces")]), ...namespaces.map(n => opt([n, n])));
    scope.value = migrationScope.namespace;
    put(status, pending ? t("engine.checking") : !visible.length && !problems.length ? t("migration.empty") : "",
      problems.length ? h("details", {}, h("summary", { text: t("migration.unavailable") }), h("ul", {}, problems.map(p => h("li", { text: p })))) : null);
    S.migrationCount = visible.length || (pending || problems.length ? null : 0);
    const badge = document.querySelector("#nav .nav-item .count");
    if (badge) { badge.textContent = S.migrationCount ?? ""; }
    if (!pending && migrationScope.cluster && migrationScope.namespace && !namespaces.includes(migrationScope.namespace)) {
      migrationScope.namespace = ""; draw();
    }
  }
  async function read(source, kind) {
    if (migrationScope.cluster) {
      const { items } = await API.kubeList(kind, true, source);
      return items.filter(({ metadata }) => !metadata.deletionTimestamp && !metadata.ownerReferences?.length).map(({ metadata }) => ({ name: metadata.name, label: metadata.name, uid: metadata.uid, namespace: metadata.namespace }));
    }
    const rows = await API.migrationList(source, kind);
    if (kind === "images") return rows.flatMap(i => {
      const tags = (i.RepoTags || []).filter(tag => tag !== "<none>:<none>");
      return (tags.length ? tags : [i.Id]).map(name => ({ name, label: name === i.Id ? shortId(name) : name, uid: i.Id }));
    });
    return rows.map(r => kind === "containers" ? { name: r.Id, label: containerName(r) } : { name: r.Name, label: r.Name });
  }
  async function reload() {
    const id = ++revision, cluster = migrationScope.cluster;
    items = []; problems = []; pending = 1;
    engines.setAttribute("aria-pressed", String(!cluster));
    clusters.setAttribute("aria-pressed", String(cluster));
    scope.hidden = !cluster;
    picker.disabled = true;
    socket.hidden = cluster || migrationScope.engine !== "custom";
    draw();
    try {
      let sources;
      if (cluster) {
        const contexts = await API.kubeContexts();
        if (!current(id)) return;
        choices = contexts.map(n => [n, n]);
        if (!contexts.includes(migrationScope.context)) migrationScope.context = contexts.includes(S.kubeContext) ? S.kubeContext : contexts[0] || "";
        picker.replaceChildren(...choices.map(opt)); picker.value = migrationScope.context;
        sources = picker.value ? [picker.value] : [];
      } else {
        const [local, contexts] = await Promise.all([API.migrationSources(), API.contexts()]);
        if (!current(id)) return;
        choices = [["podbag", "Podbag"], ["docker", "Docker"], ["podman", "Podman"], ["kiri", "Kiri"],
          ...contexts.map(n => ["context:" + n, n]), ...(navigator.userAgent.includes("Windows") ? [] : [["custom", t("engine.socket")]])];
        picker.replaceChildren(opt(["", t("migration.allEngines")]), ...choices.map(opt));
        picker.value = migrationScope.engine;
        sources = picker.value === "custom" ? [socket.value.trim()].filter(Boolean) : picker.value ? [picker.value] : local;
      }
      picker.disabled = false;
      const jobs = sources.flatMap(source => kinds().map(kind => [source, kind]));
      pending = jobs.length; draw();
      await Promise.all(jobs.map(async ([source, kind]) => {
        try {
          const rows = await read(source, kind);
          if (current(id)) {
            const sourceLabel = choices.find(([n]) => n === source)?.[1] || source;
            items.push(...rows.map(r => ({ ...r, kind, source, sourceLabel })));
          }
        } catch (e) { if (current(id)) problems.push([source, kindLabel(kind), String(e)].join(" · ")); }
        finally { if (current(id)) { pending--; draw(); } }
      }));
    } catch (e) { if (current(id)) { problems.push(String(e)); pending = 0; picker.disabled = false; draw(); } }
  }
  picker.onchange = () => {
    if (migrationScope.cluster) { migrationScope.context = picker.value; migrationScope.namespace = ""; }
    else migrationScope.engine = picker.value;
    reload();
  };
  socket.onchange = reload;
  reload();
  return { reload };
}

function confirmMigration(item, choices, reload) {
  const dialog = h("dialog", { class: "migration-dialog", "aria-labelledby": "migration-heading" });
  const mark = h("span", { class: "migration-mark", "aria-hidden": "true" });
  const title = h("h2", { id: "migration-heading", tabindex: "-1" });
  const status = h("p", { role: "status", class: "migration-status" });
  const actions = h("div", { class: "buttons" });
  const request = { kind: item.kind, source: item.source, name: item.name, uid: item.uid || "",
    sourceNamespace: item.namespace || "", destinationNamespace: "" };
  const target = h("select", { id: "migration-target", "aria-label": t("migration.to") },
    choices.filter(([value]) => value !== item.source).map(([value, text]) => h("option", { value, text })));
  const custom = migrationSocket();
  const namespace = h("input", { id: "migration-namespace", type: "text", value: item.namespace, "aria-label": t("migration.namespace") });
  const destination = h("dd", {}, target, custom);
  const destNs = h("dd", {}, namespace);
  const defaults = h("input", { type: "checkbox" });
  const defaultsField = h("label", { class: "check", hidden: item.kind !== "containers" }, defaults, t("migration.defaults"));
  const route = h("dl", { class: "fields" },
    h("dt", { text: t("migration.resource") }), h("dd", { text: item.label }),
    h("dt", { text: t("migration.from") }), h("dd", { text: [item.sourceLabel, item.namespace].filter(Boolean).join(" / ") }),
    h("dt", { text: t("migration.to") }), destination,
    item.namespace ? [h("dt", { text: t("migration.namespace") }), destNs] : null);
  const progress = h("div", { class: "migration-track", "aria-hidden": "true" }, h("span"));
  const close = () => dialog.close();
  const state = (name, glyph, heading) => {
    dialog.dataset.phase = name;
    mark.replaceChildren(icon(glyph));
    title.textContent = t(heading);
  };
  state("confirm", "migration", "migration.confirm");
  status.textContent = t("migration.preserve");
  const cancel = button(null, t("action.cancel"), close);
  const start = button("migration", t("migration.start"), async () => {
    if (dialog.dataset.phase !== "confirm") return;
    request.destination = target.value === "custom" ? custom.value.trim() : target.value;
    request.destinationNamespace = namespace.value.trim();
    request.destinationDefaults = defaults.checked;
    defaultsField.hidden = true;
    destination.textContent = target.value === "custom" ? request.destination : target.selectedOptions[0].textContent;
    destNs.textContent = request.destinationNamespace;
    state("running", "refresh", "migration.running");
    actions.replaceChildren();
    status.textContent = t("migration.prepare");
    title.focus();
    try {
      await API.migrate(request, phase => { if (dialog.dataset.phase === "running") status.textContent = t(`migration.${phase}`); });
      state("done", "check", "migration.done");
      status.textContent = t("migration.verified");
      dialog.addEventListener("close", reload, { once: true });
    } catch (err) {
      state("failed", "warning", "migration.failed");
      status.textContent = String(err);
    }
    const finish = button(null, t("migration.close"), close, "primary");
    actions.replaceChildren(finish);
    finish.focus();
  }, "primary");
  start.id = "migration-start";
  function validate() {
    custom.hidden = target.value !== "custom";
    const value = custom.hidden ? target.value : custom.value.trim();
    start.disabled = !value || value === item.source || (!!item.namespace && !namespace.value.trim());
  }
  target.onchange = custom.oninput = namespace.oninput = validate;
  validate();
  actions.append(cancel, start);
  dialog.append(h("div", { class: "migration-dialog-body" }, mark, title, route, defaultsField, progress, status, actions));
  dialog.addEventListener("cancel", e => { if (dialog.dataset.phase === "running") e.preventDefault(); });
  dialog.addEventListener("close", () => dialog.remove(), { once: true });
  document.body.append(dialog);
  dialog.showModal();
  cancel.focus();
}
