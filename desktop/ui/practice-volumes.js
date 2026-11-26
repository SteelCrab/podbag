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
    steps: [["start"], ...record.steps.map(step => [step.key, step.command])],
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
  let last, lastStage, lastCommand, timer, revision = 0;
  function updateStage(stage, command) {
    if (stage === lastStage && command === lastCommand) return;
    lastStage = stage;
    lastCommand = command;
    onStage?.(stage, command);
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
      updateStage(state?.stage || (completed ? "complete" : "start"), state?.command);
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
    const canRemove = row => state.module ? state.remove_volume === row.Name : row.Name !== "my-api" || state.stage === "remove";
    const removeButton = row => {
      const b = iconButton("remove", t("action.remove"), () => act("remove", row, target), "danger");
      b.disabled = !canRemove(row);
      return b;
    };
    if (detail) {
      put(resources, h("header", { class: "toolbar" }, iconButton("back", t("action.back"), () => { detail = null; render(); }),
        h("h2", { text: detail.Name }), removeButton(detail)), infoPanel("volumes", detail));
    } else {
      put(resources, state.volumes.length
        ? resourceTable("volumes", state.volumes, row => act("inspect", row, target), row => h("div", { class: "actions" }, removeButton(row)))
        : h("p", { class: "nothing", text: t("empty.volumes.title") }));
    }
  }
  async function act(action, row, session) {
    if (busy) return;
    busy = true;
    try {
      if (action === "remove" && !(await confirmDialog(t("confirm.removeVolume", { name: row.Name })))) return;
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
  let state, detailId, busy = false;
  const refresh = watchPractice(resources, practiceResources(containerPractice), next => {
    const id = next?.module || containerPractice.id;
    if (guide.dataset.module !== id) {
      put(guide, trainingGuideButton(id));
      guide.dataset.module = id;
    }
    if (next?.session !== state?.session) detailId = null;
    state = next;
    render();
  });

  function render() {
    if (!state) {
      practiceStartOptions(resources, S.trainingRecords.filter(record => record.id === containerPractice.id || record.steps?.length));
      return;
    }
    const canStop = row => state.module ? state.stop === containerName(row) : state.stage === "stop";
    const resourceAction = row => canStop(row) && iconButton("stop", t("action.stop"),
      () => change("stop", row.Id, state.session));
    const detail = state.containers.find(row => row.Id === detailId);
    if (detail) {
      put(resources, h("header", { class: "toolbar" },
        iconButton("back", t("action.back"), () => { detailId = null; render(); }),
        h("h2", { text: containerName(detail) }), resourceAction(detail)), infoPanel("containers", detail),
        state.web?.container === containerName(detail) ? renderPorts(detail) : null);
      return;
    }
    put(resources, state.containers.length
      ? resourceTable("containers", state.containers, openContainer,
        row => h("div", { class: "actions" }, resourceAction(row)))
      : h("p", { class: "nothing", text: t("empty.containers.title") }));
  }
  function openContainer(row) {
    detailId = row.Id;
    render();
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
      await (state.module ? courseRequest : containerPractice.request)(action, session, id);
    } catch (e) { toast(String(e), true); }
    finally { busy = false; await refresh(); }
  }
}
