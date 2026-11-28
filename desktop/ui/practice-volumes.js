"use strict";

const GUI_VOLUME_FORMAT = "podbag-cli-gui-volumes-v1";
const volumePractice = {
  id: "volume-basics",
  namespace: "practice.",
  steps: [
    ["start"], ["create", "podbag volume create my-api"], ["inspect"], ["remove"],
    ["verify", "podbag volume ls"], ["finish", "exit"],
  ],
  request: (...args) => API.practiceVolumes(...args),
};
const containerPractice = {
  id: "container-basics",
  namespace: "training.container.",
  steps: [
    ["start"],
    ["run", "podbag run -d --name hello-web --network none docker.io/library/busybox:1.37.0 httpd -f -p 8080"],
    ["stop"], ["check", "podbag ps -a"], ["finish", "exit"],
  ],
  request: (action, session, name) => API.practiceVolumes(action, session, name, true),
};

function courseRequest(action = "status", session = null, name = null) {
  return API.practiceVolumes(action, session, name, false, true);
}

function courseUsesVolumes(record) {
  return record.steps?.some(step => step.key === "inspect");
}

function practiceFor(record) {
  if (record.id === volumePractice.id) return volumePractice;
  if (record.id === containerPractice.id) return containerPractice;
  if (!record.steps?.length) return null;
  return {
    id: record.id,
    namespace: "training.course.",
    steps: [["start"], ...record.steps.map(step => [step.key, step.command, step.ui])],
    // The module file names its steps; the app names only the start.
    label(key) {
      if (key === "start") return t("training.course.start");
      const names = record.steps.find(step => step.key === key)?.names || {};
      return names[I18N.lang] || names.en || key;
    },
    async request() {
      const state = await courseRequest();
      return state?.module === record.id ? state : null;
    },
  };
}

// The buttons of a module `ui` step. A row button sits on the row named by `ui.name`.
// Both send the name, never the ID.
const STEP_ROW_BUTTONS = {
  start: ["start", "action.start"], stop: ["stop", "action.stop"], restart: ["restart", "action.restart"],
  pause: ["pause", "action.pause"], resume: ["start", "action.resume"],
  remove: ["remove", "action.remove"], logs: ["logs", "action.logs"],
};
const STEP_TOOLBAR_ICONS = {
  pull: "pull", "remove-image": "remove", "create-volume": "volumes",
  "remove-network": "remove", prune: "prune", inspect: "inspect",
};
const STEP_PRUNE_QUESTIONS = {
  containers: "confirm.pruneContainers", images: "confirm.pruneImages",
  volumes: "confirm.pruneVolumes", networks: "confirm.pruneNetworks",
};

// The action of the step when it belongs on the row named ROW_NAME and is one of ACTIONS.
function stepRowAction(state, rowName, actions) {
  return state.module && state.ui?.name === rowName && actions.includes(state.ui.action) ? state.ui.action : null;
}

// The step's button in the toolbar, when its action has no row on the page.
// `rows` are the names of the rows of the page; `change(action, name)` sends the request.
function practiceStepButton(state, change, rows) {
  const ui = state.module && state.ui;
  const icon = ui && STEP_TOOLBAR_ICONS[ui.action];
  if (!icon || ui.action === "inspect" && rows.includes(ui.name)) return null;
  const { action, name } = ui;
  const question = {
    "remove-image": () => t("confirm.removeImage", { name }),
    "remove-network": () => t("confirm.removeNetwork", { name }),
    prune: () => t(STEP_PRUNE_QUESTIONS[name] || "confirm.pruneContainers"),
  }[action];
  return h("div", { class: "buttons-row" }, button(icon, t("training.ui.button." + action, { name }), async () => {
    if (question && !(await confirmDialog(question(), action === "prune" ? t("action.prune") : undefined))) return;
    change(action, name);
  }));
}

function practiceLabel(practice, key) {
  return practice.label ? practice.label(key) : t(practice.namespace + key);
}

function practiceResources(legacy) {
  return { async request() {
    return (await courseRequest()) || legacy.request();
  } };
}

function practiceStartOptions(resources, records) {
  put(resources, h("p", { text: t("training.terminalStart") }), records.map(record =>
    h("section", { class: "practice-start-option" },
      h("h2", { text: titleOf(trainingWords(record.package)) }), codeLine(practiceStartCommand(record.id)))));
}

function practiceStartCommand(id) { return "podbag lab start " + id; }

function trainingGuideButton(id, style) {
  return button("globe", t("training.guide"), async () => {
    try { await API.openTrainingGuide(id); } catch (e) { toast(String(e), true); }
  }, style);
}

// Poll only while this view is mounted. Redraw on transitions so keyboard focus stays on the row or action.
function watchPractice(panel, practice, onChange, { completed, onStage } = {}) {
  let last, lastStage, lastCommand, lastUi, timer, revision = 0;
  function updateStage(stage, command, ui) {
    const uiKey = JSON.stringify(ui);
    if (stage === lastStage && command === lastCommand && uiKey === lastUi) return;
    lastStage = stage;
    lastCommand = command;
    lastUi = uiKey;
    onStage?.(stage, command, ui);
  }
  const error = h("p", { role: "alert", class: "error-text" });
  panel.after(error);
  async function refresh() {
    clearTimeout(timer);
    if (!panel.isConnected) return;
    const ticket = ++revision;
    try {
      const state = await practice.request();
      // Completion is saved by the shell after it shuts down the practice engine.
      if (!state && onStage) completed = !!(await API.volumeToolsLab("summary", null, practice.id))[0]?.completed;
      if (!panel.isConnected || ticket !== revision) return;
      updateStage(state?.stage || (completed ? "complete" : "start"), state?.command, state?.ui);
      const key = JSON.stringify(state);
      if (last !== key) onChange?.(state);
      last = key;
      error.textContent = "";
    } catch (e) {
      if (panel.isConnected && ticket === revision) { error.textContent = String(e); last = undefined; onChange?.(null); }
    } finally {
      if (panel.isConnected && ticket === revision) timer = setTimeout(refresh, 800);
    }
  }
  updateStage(completed ? "complete" : "start");
  refresh();
  return refresh;
}

function renderPracticeVolumes(main) {
  const resources = h("section", { class: "practice-resources" });
  const guide = h("div");
  const holder = h("div", { class: "content practice-workspace" }, resources);
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("practice.volumes") }), guide), holder);
  let state, detail, busy = false;
  put(guide, trainingGuideButton(volumePractice.id));
  const refresh = watchPractice(resources, practiceResources(volumePractice), next => {
    const id = next?.module || volumePractice.id;
    if (guide.dataset.module !== id) {
      put(guide, trainingGuideButton(id));
      guide.dataset.module = id;
    }
    if (!next || next.session !== state?.session || !next.volumes.some(v => v.Name === detail?.Name)) detail = null;
    state = next;
    render();
  });
  function render() {
    if (!state) {
      practiceStartOptions(resources, S.trainingRecords.filter(record => record.id === volumePractice.id || courseUsesVolumes(record)));
      return;
    }
    const target = state.session;
    const stepRemoves = row => stepRowAction(state, row.Name, ["remove-volume"]);
    const canRemove = row => state.module ? state.remove_volume === row.Name || stepRemoves(row) : row.Name !== "my-api" || state.stage === "remove";
    const removeButton = row => {
      const b = iconButton("remove", t("action.remove"), () => act(stepRemoves(row) ? "remove-volume" : "remove", row, target), "danger");
      b.disabled = !canRemove(row);
      return b;
    };
    const step = practiceStepButton(state, (action, name) => act(action, { Name: name }, target), state.volumes.map(v => v.Name));
    if (detail) {
      put(resources, h("header", { class: "toolbar" }, iconButton("back", t("action.back"), () => { detail = null; render(); }),
        h("h2", { text: detail.Name }), removeButton(detail)), step, infoPanel("volumes", detail));
    } else {
      put(resources, step, state.volumes.length
        ? resourceTable("volumes", state.volumes, row => act("inspect", row, target), row => h("div", { class: "actions" }, removeButton(row)))
        : h("p", { class: "nothing", text: t("empty.volumes.title") }));
    }
  }
  async function act(action, row, session) {
    if (busy) return;
    busy = true;
    try {
      if ((action === "remove" || action === "remove-volume") && !(await confirmDialog(t("confirm.removeVolume", { name: row.Name })))) return;
      if (!resources.isConnected || state?.session !== session) return;
      const result = await (state.module ? courseRequest : volumePractice.request)(action, session, row.Name);
      if (!resources.isConnected) return;
      detail = result.detail;
      state = result;
      render();
      await refresh();
    } catch (e) { toast(String(e), true); await refresh(); }
    finally { busy = false; }
  }
}

function renderPracticeContainers(main) {
  const resources = h("section", { class: "practice-resources" });
  const guide = h("div", {}, trainingGuideButton(containerPractice.id));
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("training.containers") }), guide),
  h("div", { class: "content practice-workspace" }, resources));
  // log: the text of a logs step, { id, stage, text }: shown only under container `id`,
  // and dropped when the step moves on from `stage` or the container is gone.
  // inspected: the result of the toolbar's inspect step (a network or a volume), { stage,
  // detail }: dropped when the step moves on from `stage`.
  let state, detailId, log = null, inspected = null, busy = false;
  const refresh = watchPractice(resources, practiceResources(containerPractice), next => {
    const id = next?.module || containerPractice.id;
    if (guide.dataset.module !== id) {
      put(guide, trainingGuideButton(id));
      guide.dataset.module = id;
    }
    if (next?.session !== state?.session) { detailId = null; log = null; inspected = null; }
    if (next?.text) log = { id: detailId, stage: next.stage, text: next.text };
    if (inspected && next?.stage !== inspected.stage) inspected = null;
    if (log && (next?.stage !== log.stage || !next.containers.some(row => row.Id === log.id))) log = null;
    state = next;
    render();
  });

  function render() {
    if (!state) {
      practiceStartOptions(resources, S.trainingRecords.filter(record => record.id === containerPractice.id || record.steps?.length));
      return;
    }
    const canStop = row => state.module ? state.stop === containerName(row) : state.stage === "stop";
    const resourceAction = row => {
      const step = stepRowAction(state, containerName(row), Object.keys(STEP_ROW_BUTTONS));
      if (step) {
        const [name, label] = STEP_ROW_BUTTONS[step];
        return iconButton(name, t(label), () => stepRow(step, row), step === "remove" ? "danger" : "");
      }
      return canStop(row) && iconButton("stop", t("action.stop"), () => change("stop", row.Id, state.session));
    };
    const session = state.session;
    const stepButton = practiceStepButton(state, async (action, name) => {
      const result = await change(action, name, session);
      if (action === "inspect" && result?.detail && current(result, session)) {
        inspected = { stage: result.stage, detail: result.detail };
        render();
      }
    }, state.containers.map(containerName));
    const shown = inspected && [h("h2", { class: "panel-title", text: inspected.detail.Name }),
      infoPanel(inspected.detail.Mountpoint ? "volumes" : "networks", inspected.detail)];
    const detail = state.containers.find(row => row.Id === detailId);
    if (detail) {
      put(resources, h("header", { class: "toolbar" },
        iconButton("back", t("action.back"), () => { detailId = null; log = null; render(); }),
        h("h2", { text: containerName(detail) }), resourceAction(detail)), stepButton, shown, infoPanel("containers", detail),
        log?.id === detail.Id ? h("pre", { class: "lab-output", text: log.text }) : null,
        state.web?.container === containerName(detail) ? renderPorts(detail) : null);
      return;
    }
    put(resources, stepButton, shown, state.containers.length
      ? resourceTable("containers", state.containers, openContainer,
        row => h("div", { class: "actions" }, resourceAction(row)))
      : h("p", { class: "nothing", text: t("empty.containers.title") }));
  }
  async function stepRow(action, row) {
    const name = containerName(row);
    // The row's session: `change` refuses the request if polling replaces it during the question.
    const session = state.session;
    if (action === "remove") {
      const running = ["running", "paused", "restarting"].includes(row.State);
      if (!(await confirmDialog(t(running ? "confirm.removeRunning" : "confirm.removeContainer", { name })))) return;
    }
    const result = await change(action, name, session);
    if (action === "logs" && result?.text && current(result, session)) {
      log = { id: row.Id, stage: result.stage, text: result.text };
      detailId = row.Id;
      render();
    }
  }
  // A reply still for the page's session and step: `change` refreshes before it returns, and
  // output of an earlier step must not come back after that refresh dropped it.
  function current(result, session) {
    return state?.session === session && state.stage === result.stage;
  }
  function openContainer(row) {
    if (log?.id !== row.Id) log = null;
    detailId = row.Id;
    render();
    if (state.module && state.ui?.action === "inspect" && state.ui.name === containerName(row)) {
      change("inspect", containerName(row), state.session);
    }
    if (state.inspect_container === containerName(row)) change("inspect-failed", row.Id, state.session);
  }
  function renderPorts(detail) {
    const { web, session } = state;
    return h("section", { class: "card practice-ports" },
      h("h2", { text: t("col.ports") }),
      web.ready ? h("div", {}, h("p", { text: "127.0.0.1:" + web.port + " → 8080/tcp" }), codeLine(web.url),
        h("div", { class: "buttons-row" }, button("globe", t("training.openBrowser"),
          () => change("open-web", detail.Id, session), "primary")))
        : button(null, t("training.viewPorts"), () => change("ports", detail.Id, session)));
  }
  async function change(action, id, session) {
    if (busy) return;
    busy = true;
    try {
      if (state?.session !== session) return;
      return await (state.module ? courseRequest : containerPractice.request)(action, session, id);
    } catch (e) { toast(String(e), true); }
    finally { busy = false; await refresh(); }
  }
}
