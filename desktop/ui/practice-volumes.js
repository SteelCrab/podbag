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

function practiceStartCommand(id) { return "podbag lab start " + id; }

function trainingGuideButton(id, style) {
  return button("globe", t("training.guide"), async () => {
    try { await API.openTrainingGuide(id); } catch (e) { toast(String(e), true); }
  }, style);
}

// Poll only while this view is mounted. Redraw on transitions so keyboard focus stays on the row or action.
function watchPractice(panel, practice, onChange, { completed, onStage } = {}) {
  let last, lastStage, timer, revision = 0;
  function updateStage(stage) {
    if (stage === lastStage) return;
    lastStage = stage;
    onStage?.(stage);
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
      updateStage(state?.stage || (completed ? "complete" : "start"));
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
  const holder = h("div", { class: "content practice-workspace" }, resources);
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("practice.volumes") }), trainingGuideButton(volumePractice.id)), holder);
  let state, detail, busy = false;
  const refresh = watchPractice(resources, volumePractice, next => {
    if (!next || next.session !== state?.session || !next.volumes.some(v => v.Name === detail?.Name)) detail = null;
    state = next;
    render();
  });
  function render() {
    if (!state) {
      put(resources, h("p", { text: t("training.terminalStart") }), codeLine(practiceStartCommand(volumePractice.id)));
      return;
    }
    const target = state.session;
    const canRemove = row => row.Name !== "my-api" || state.stage === "remove";
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
      const result = await API.practiceVolumes(action, session, row.Name);
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
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("training.containers") }), trainingGuideButton(containerPractice.id)),
  h("div", { class: "content practice-workspace" }, resources));
  let state, detailId, busy = false;
  const refresh = watchPractice(resources, containerPractice, next => {
    if (next?.session !== state?.session) detailId = null;
    state = next;
    render();
  });

  function render() {
    if (!state) {
      put(resources, h("p", { text: t("training.terminalStart") }), codeLine(practiceStartCommand(containerPractice.id)));
      return;
    }
    const action = state.stage === "stop" ? "stop" : null;
    const resourceAction = row => action && iconButton(action, t("action." + action),
      () => change(action, row.Id, state.session));
    const detail = state.containers.find(row => row.Id === detailId);
    if (detail) {
      put(resources, h("header", { class: "toolbar" },
        iconButton("back", t("action.back"), () => { detailId = null; render(); }),
        h("h2", { text: containerName(detail) }), resourceAction(detail)), infoPanel("containers", detail));
      return;
    }
    put(resources, state.containers.length
      ? resourceTable("containers", state.containers, row => { detailId = row.Id; render(); },
        row => h("div", { class: "actions" }, resourceAction(row)))
      : h("p", { class: "nothing", text: t("empty.containers.title") }));
  }
  async function change(action, id, session) {
    if (busy) return;
    busy = true;
    try {
      await containerPractice.request(action, session, id);
    } catch (e) { toast(String(e), true); }
    finally { busy = false; await refresh(); }
  }
}
