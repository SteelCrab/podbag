"use strict";

const GUI_VOLUME_FORMAT = "podbag-cli-gui-volumes-v1";
const volumePractice = {
  id: "volume-basics",
  namespace: "practice.",
  steps: ["start", "create", "inspect", "remove", "verify", "finish"],
  request: (...args) => API.practiceVolumes(...args),
};
const containerPractice = {
  id: "container-basics",
  namespace: "training.container.",
  steps: ["start", "run", "stop", "check", "finish"],
  request: (action, session, name) => API.practiceVolumes(action, session, name, true),
};

function showPracticeProgress(panel, stage, practice) {
  if (panel.dataset.stage === stage) return;
  panel.dataset.stage = stage;
  const { namespace, steps } = practice;
  const index = stage === "complete" ? steps.length : steps.indexOf(stage === "restart" ? "stop" : stage);
  const items = steps.map((step, i) => {
    const state = i < index ? "done" : i === index ? "current" : "pending";
    return h("li", { class: "practice-" + state, "aria-current": state === "current" ? "step" : null },
      h("span", { class: "practice-marker", "aria-hidden": "true" }, state === "done" && icon("check")),
      h("span", { class: "practice-step-name", text: t(namespace + step) }),
      h("small", { text: t("practice." + state) }));
  });
  const outcome = stage === "complete" || stage === "restart";
  put(panel, h("h2", { text: t("practice.progress") }), h("ol", { class: "practice-steps", role: "list" }, items),
    h("p", { role: "status", class: outcome ? "" : "visually-hidden",
      text: t(outcome ? namespace + "hint." + stage : namespace + stage) }));
}

function showModuleStatus(status, stage) {
  const label = stage === "complete" ? "tutorial.complete"
    : stage === "start" ? "tutorial.installed" : "practice.current";
  const text = t(label);
  if (status.textContent !== text) status.textContent = text;
}

function trainingGuideButton(id, style) {
  return button("globe", t("training.guide"), async () => {
    try { await API.openTrainingGuide(id); } catch (e) { toast(String(e), true); }
  }, style);
}

// Poll only while this view is mounted. Redraw on transitions so keyboard focus stays on the row or action.
function watchPractice(panel, completed, onChange, practice = volumePractice, draw = showPracticeProgress) {
  let last, timer, revision = 0;
  const error = h("p", { role: "alert", class: "error-text" });
  panel.after(error);
  async function refresh() {
    clearTimeout(timer);
    if (!panel.isConnected) return;
    const ticket = ++revision;
    try {
      const state = await practice.request();
      // Completion is saved by the shell after it shuts down the practice engine.
      if (!state) completed = !!(await API.volumeToolsLab("summary", null, practice.id))[0]?.completed;
      if (!panel.isConnected || ticket !== revision) return;
      const stage = state?.stage || (completed ? "complete" : "start");
      draw(panel, stage, practice);
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
  draw(panel, completed ? "complete" : "start", practice);
  refresh();
  return refresh;
}

function renderPracticeVolumes(main) {
  const panel = h("section", { class: "practice-progress card" });
  const resources = h("section", { class: "practice-resources" });
  const terminal = button("shell", t("status.terminal"), async () => {
    try { await API.volumeToolsLab("start", null, volumePractice.id); } catch (e) { toast(String(e), true); }
  });
  const holder = h("div", { class: "content practice-workspace" }, resources, panel);
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("practice.volumes") }), trainingGuideButton(volumePractice.id), terminal), holder);
  let state, detail, busy = false;
  const refresh = watchPractice(panel, false, next => {
    if (!next || next.session !== state?.session || !next.volumes.some(v => v.Name === detail?.Name)) detail = null;
    state = next;
    terminal.hidden = !!next;
    render();
  });
  function render() {
    resources.hidden = !state;
    if (!state) { resources.replaceChildren(); return; }
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
      if (!panel.isConnected || state?.session !== session) return;
      const result = await API.practiceVolumes(action, session, row.Name);
      if (!panel.isConnected) return;
      detail = result.detail;
      state = result;
      render();
      await refresh();
    } catch (e) { toast(String(e), true); await refresh(); }
    finally { busy = false; }
  }
}

function renderPracticeContainers(main) {
  const panel = h("section", { class: "practice-progress card" });
  const resources = h("section", { class: "practice-resources" });
  const terminal = button("shell", t("status.terminal"), async () => {
    try { await API.volumeToolsLab("start", null, containerPractice.id); } catch (e) { toast(String(e), true); }
  });
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("training.containers") }), trainingGuideButton(containerPractice.id), terminal),
  h("div", { class: "content practice-workspace" }, resources, panel));
  let state, detailId, busy = false;
  const refresh = watchPractice(panel, false, next => {
    if (next?.session !== state?.session) detailId = null;
    state = next;
    terminal.hidden = !!next;
    render();
  }, containerPractice);

  function render() {
    resources.hidden = !state;
    if (!state) { resources.replaceChildren(); return; }
    const action = state.stage === "restart" ? "start" : state.stage === "stop" ? "stop" : null;
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
