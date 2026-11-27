// Desktop's local model controls. Only Start starts a backend, and only Pull downloads.
"use strict";

const Models = { status: null, pending: "", error: null, generation: 0, name: "", selected: "", prompt: "", response: "" };

function openModelRunner(main) {
  renderModelRunner(main);
  modelAction("status");
}

async function modelAction(action, fields = {}) {
  if (Models.pending && action !== "stop") return;
  const generation = ++Models.generation;
  Models.pending = action;
  Models.error = null;
  if (action === "run") Models.response = "";
  showModelProgress();
  try {
    const result = await API.models({ action, ...fields });
    if (generation !== Models.generation) return;
    if (action === "run") {
      Models.response = result.response;
      const status = await API.models({ action: "status" });
      if (generation !== Models.generation) return;
      Models.status = status;
    } else {
      Models.status = result;
      if (action === "pull") Models.name = "";
    }
  } catch (error) {
    if (generation !== Models.generation) return;
    if (["missing", "start", "stopped", "modelName", "notInstalled", "prompt", "backend", "busy"].includes(error?.kind)) {
      Models.error = error;
    } else {
      Models.error = { kind: "backend" };
    }
    if (["start", "stopped"].includes(Models.error.kind)) Models.status = null;
  } finally {
    if (generation === Models.generation) {
      Models.pending = "";
      showModelProgress();
    }
  }
}

function showModelProgress() {
  if (S.view === "models") renderModelRunner(document.getElementById("main"));
}

function renderModelRunner(main) {
  const { status, pending, error } = Models;
  const running = !!status?.running;
  const enable = button(running ? "stop" : "start", t(running ? "models.stop" : "models.start"),
    () => modelAction(running ? "stop" : "start"), running ? "" : "primary");
  enable.disabled = running ? pending === "stop" : !!pending;
  const refresh = iconButton("refresh", t("action.refresh"), () => modelAction("status"));
  refresh.disabled = !!pending;
  put(main,
    h("header", { class: "toolbar" }, h("h1", { text: t("nav.models") }), h("div", { class: "tools" }, running ? refresh : null, enable)),
    h("div", { class: "content model-runner" },
      h("div", { class: "model-runtime" },
        h("span", { class: `model-runner-state ${running ? "running" : ""}`, role: "status", text: t(running ? "models.running" : "models.stopped") }),
        running ? h("span", { class: "secondary-text", text: `Ollama ${status.version}` }) : null),
      !running ? h("p", { class: "hint", text: t("models.hint") }) : null,
      pending ? h("p", { class: "hint", role: "status", text: t(`models.progress.${pending}`) }) : null,
      error ? h("div", { class: "model-error", role: "alert" },
        h("p", { class: "error-text", text: t(`models.error.${error.kind}`) }),
        error.detail ? h("p", { class: "hint", text: error.detail }) : null) : null,
      running ? installedModels(status.models, !!pending) : null,
      running && status.models.length ? modelPrompt(status.models, !!pending) : null));
}

function installedModels(models, busy) {
  const source = h("input", { id: "model-name", type: "text", placeholder: "smollm2:135m",
    required: true, autocomplete: "off", spellcheck: "false", disabled: busy,
    oninput: (event) => { Models.name = event.target.value; } });
  source.value = Models.name;
  const download = button("pull", t("models.pull"), null);
  download.type = "submit";
  download.disabled = busy;
  return h("section", { class: "card model-card" },
    h("h2", { class: "panel-title", text: t("models.installed") }),
    h("form", { class: "model-pull", onsubmit: (event) => {
      event.preventDefault();
      modelAction("pull", { model: source.value });
    } },
    h("label", { for: "model-name" }, t("models.registryName")),
    h("div", { class: "model-inline" }, source, download)),
    models.length ? h("div", { class: "models-table" },
      h("table", {},
        h("thead", {}, h("tr", {}, [t("models.model"), t("col.size"), t("detail.status")].map((text) => h("th", { scope: "col", text })), h("th", {}))),
        h("tbody", {}, models.map((model) => {
          const unload = model.loaded ? button(null, t("models.unload"), () => modelAction("unload", { model: model.name })) : null;
          if (unload) unload.disabled = busy;
          return h("tr", {},
            h("td", { class: "mono", text: model.name }),
            h("td", { text: size(model.size) }),
            h("td", { text: t(model.loaded ? "models.loaded" : "models.idle") }),
            h("td", {}, unload));
        })))) : h("p", { class: "hint", text: t("models.empty") }));
}

function modelPrompt(models, busy) {
  if (!models.some((model) => model.name === Models.selected)) Models.selected = models[0].name;
  const select = h("select", { id: "model-select", disabled: busy, onchange: (event) => { Models.selected = event.target.value; } },
    models.map((model) => h("option", { value: model.name, text: model.name })));
  select.value = Models.selected;
  const prompt = h("textarea", { id: "model-prompt", rows: 4, maxlength: 16000, required: true, disabled: busy,
    oninput: (event) => { Models.prompt = event.target.value; } });
  prompt.value = Models.prompt;
  const run = button("start", t("models.run"), null, "primary");
  run.type = "submit";
  run.disabled = busy;
  return h("section", { class: "card model-card" },
    h("h2", { class: "panel-title", text: t("models.generate") }),
    h("form", { class: "model-prompt", onsubmit: (event) => {
      event.preventDefault();
      modelAction("run", { model: select.value, prompt: prompt.value });
    } },
    h("label", { for: "model-select" }, t("models.model")), select,
    h("label", { for: "model-prompt" }, t("models.prompt")), prompt,
    h("div", {}, run)),
    Models.response ? h("div", { class: "model-answer" },
      h("h3", { text: t("models.response") }), h("pre", { text: Models.response, tabindex: "0" })) : null);
}
