"use strict";

const GUI_VOLUME_FORMAT = "podbag-cli-gui-volumes-v1";
const volumePractice = {
  id: "volume-basics",
  namespace: "practice.",
  steps: ["start", "create", "inspect", "remove", "verify", "finish"],
  commands: { create: "podbag volume create my-api", verify: "podbag volume ls", finish: "exit" },
  request: (...args) => API.practiceVolumes(...args),
};
const containerPractice = {
  id: "container-basics",
  namespace: "training.container.",
  steps: ["start", "run", "stop", "check", "finish"],
  commands: { run: "podbag run -d --name hello-web --network none docker.io/library/busybox:1.37.0 httpd -f -p 8080",
    check: "podbag ps -a", finish: "exit" },
  request: (action, session, name) => API.practiceVolumes(action, session, name, true),
};

function showPracticeProgress(panel, stage, practice) {
  if (panel.dataset.stage === stage) return;
  panel.dataset.stage = stage;
  const { namespace, steps, commands } = practice;
  const index = stage === "complete" ? steps.length : steps.indexOf(stage === "restart" ? "stop" : stage);
  const items = steps.map((step, i) => {
    const state = i < index ? "done" : i === index ? "current" : "pending";
    return h("li", { class: "practice-" + state, "aria-current": state === "current" ? "step" : null },
      h("span", { text: t(namespace + step) }), h("small", { text: t("practice." + state) }));
  });
  const command = commands[stage];
  put(panel, h("h2", { text: t("practice.progress") }), h("ul", { class: "practice-steps" }, items),
    h("p", { role: "status", text: t(namespace + "hint." + stage) }), command && codeLine(command));
}

// Poll only while this view is mounted. Redraw on transitions so keyboard focus stays on the row or action.
function watchPractice(panel, completed, onChange, practice = volumePractice) {
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
      showPracticeProgress(panel, stage, practice);
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
  showPracticeProgress(panel, completed ? "complete" : "start", practice);
  refresh();
  return refresh;
}

function renderPracticeVolumes(main) {
  const panel = h("section", { class: "practice-progress card" });
  const resources = h("section", { class: "practice-resources" });
  const terminal = button("shell", t("status.terminal"), async () => {
    try { await API.volumeToolsLab("start", null, volumePractice.id); } catch (e) { toast(String(e), true); }
  });
  const holder = h("div", { class: "content practice-workspace" }, panel, resources);
  put(main, h("header", { class: "toolbar" },
    iconButton("back", t("nav.tutorials"), () => go("tutorials")),
    h("h1", { text: t("practice.volumes") }), terminal), holder);
  let state, detail, busy = false;
  const refresh = watchPractice(panel, false, next => {
    if (!next || next.session !== state?.session || !next.volumes.some(v => v.Name === detail?.Name)) detail = null;
    state = next;
    terminal.hidden = !!next;
    render();
  });
  function render() {
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
      put(resources, h("h2", { text: t("nav.volumes") }), state.volumes.length
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
    h("h1", { text: t("training.containers") }), terminal),
  h("div", { class: "content practice-workspace" }, panel, resources));
  let state, detailId, busy = false;
  const refresh = watchPractice(panel, false, next => {
    if (next?.session !== state?.session) detailId = null;
    state = next;
    terminal.hidden = !!next;
    render();
  }, containerPractice);

  function render() {
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
    put(resources, h("h2", { text: t("nav.containers") }), state.containers.length
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
