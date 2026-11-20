"use strict";

const GUI_VOLUME_FORMAT = "podbag-cli-gui-volumes-v1";
const practiceSteps = ["start", "create", "inspect", "remove", "verify", "finish"];

function showPracticeProgress(panel, stage) {
  if (panel.dataset.stage === stage) return;
  panel.dataset.stage = stage;
  const index = stage === "complete" ? practiceSteps.length : practiceSteps.indexOf(stage);
  const items = practiceSteps.map((step, i) => {
    const state = i < index ? "done" : i === index ? "current" : "pending";
    return h("li", { class: "practice-" + state, "aria-current": state === "current" ? "step" : null },
      h("span", { text: t("practice." + step) }), h("small", { text: t("practice." + state) }));
  });
  const command = { create: "podbag volume create my-api", verify: "podbag volume ls", finish: "exit" }[stage];
  put(panel, h("h2", { text: t("practice.progress") }), h("ul", { class: "practice-steps" }, items),
    h("p", { role: "status", text: t("practice.hint." + stage) }), command && codeLine(command));
}

// Poll only while this view is mounted. Redraw on transitions so keyboard focus stays on the row or action.
function watchPractice(panel, completed, onChange) {
  let last, timer, revision = 0;
  const error = h("p", { role: "alert", class: "error-text" });
  panel.after(error);
  async function refresh() {
    clearTimeout(timer);
    if (!panel.isConnected) return;
    const ticket = ++revision;
    try {
      const state = await API.practiceVolumes();
      // Completion is saved by the shell after it shuts down the practice engine.
      if (!state) completed = !!(await API.volumeToolsLab("summary"))?.completed;
      if (!panel.isConnected || ticket !== revision) return;
      const stage = state?.stage || (completed ? "complete" : "start");
      showPracticeProgress(panel, stage);
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
  showPracticeProgress(panel, completed ? "complete" : "start");
  refresh();
  return refresh;
}

function renderPracticeVolumes(main) {
  const panel = h("section", { class: "practice-progress card" });
  const resources = h("section", { class: "practice-resources" });
  const terminal = button("shell", t("status.terminal"), async () => {
    try { await API.volumeToolsLab("start"); } catch (e) { toast(String(e), true); }
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
