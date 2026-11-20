// A lesson is a data file. Its examples and translations are installed together.
"use strict";

const TUTORIAL_KEY = "podbag.tutorial.volume-api.v1";
const TUTORIAL_LIMIT = 64 * 1024;

function validTutorial(pack) {
  const text = (value) => typeof value === "string" && value.trim().length > 0;
  if (pack?.format !== "podbag-volume-api-lab-v1" || !text(pack.starter)) return false;
  const starter = JSON.parse(pack.starter);
  if (!starter || Array.isArray(starter) || typeof starter !== "object") return false;
  return pack.translations?.en && Object.values(pack.translations).every((words) =>
    ["title", "goal", "create", "write", "serve", "request"].every((key) => text(words?.[key])));
}

const JSON_LESSON = "volume-api";
const TRAINING_FORMATS = new Map([
  ["podbag-container-basics-v1", "container-basics"],
  ["podbag-web-basics-v1", "web-basics"],
  ["podbag-volume-persistence-v1", "volume-persistence"],
  ["podbag-web-service-lifecycle-v1", "web-service-lifecycle"],
  [GUI_VOLUME_FORMAT, "volume-basics"],
  ["podbag-cli-tui-volumes-v1", "tui-volumes"],
]);

function trainingView(id) { return "training:" + id; }

function savedJsonLesson() {
  try {
    const saved = JSON.parse(localStorage.getItem(TUTORIAL_KEY));
    if (validTutorial(saved?.pack) && typeof saved.complete === "boolean") return saved;
  } catch { /* Installing a module can replace an unreadable record. */ }
  return null;
}

const trainingWords = (pack) => pack.translations[I18N.lang] || pack.translations.en;
const titleOf = (lesson) => lesson.title.replace(/^(Lab|실습|実習|实验|Práctica|Bài thực hành) 1 · /u, "$1 · ");

function trainingEntries() {
  const entries = S.trainingRecords.map((r) => ({ id: r.id, pack: r.package }));
  const json = savedJsonLesson();
  if (json) entries.push({ id: JSON_LESSON, pack: json.pack });
  return entries.map(({ id, pack }) => {
    const title = titleOf(trainingWords(pack));
    const label = title.replace(/^(?:Podbag Training|Lab|실습|実習|实验|Práctica|Bài thực hành) · /u, "");
    return { id, title, label };
  });
}

function setInstalledPractice(records) {
  S.trainingRecords = records;
  S.practiceInstalled = records.some(r => r.id === volumePractice.id || courseUsesVolumes(r));
  S.practiceContainers = records.some(r => r.id === containerPractice.id || r.steps?.length);
}

function renderPracticeProgress(panel, announcement, practice, stage, liveCommand) {
  const complete = stage === "complete";
  const current = practice.steps.findIndex(([key]) => key === (stage === "restart" ? "stop" : stage));
  const label = (key) => t(practice.namespace + key);
  const rows = practice.steps.map(([key, command], index) => {
    const done = complete || index < current;
    const active = index === current;
    const line = active && (stage === "restart" ? "exit" : key === "start" ? practiceStartCommand(practice.id) : (liveCommand || command));
    return h("li", { class: "training-step", "data-state": done ? "done" : active ? "current" : "pending",
      "aria-current": active ? "step" : null },
      h("div", { class: "training-step-heading" },
        h("span", { class: "training-step-marker", "aria-hidden": "true" }, done ? icon("check") : null),
        h("span", { class: "training-step-name", text: label(key) }),
        h("span", { class: "training-step-status",
          text: t(done ? "training.done" : active ? "practice.current" : "training.pending") })),
      line ? h("div", { class: "training-step-command" },
        stage === "restart" ? h("p", { text: t("training.restart") })
          : key === "start" ? h("p", { text: t("training.terminalStart") }) : null,
        codeLine(line)) : null);
  });
  announcement.textContent = complete ? t("tutorial.complete")
    : practice.steps[current] ? label(practice.steps[current][0]) + " · " + t("practice.current") : "";
  put(panel,
    complete ? h("p", { class: "tutorial-progress", text: t("tutorial.complete") }) : null,
    h("ol", { class: "training-steps", role: "list" }, rows),
    complete ? h("div", { class: "training-repeat" }, h("p", { text: t("training.terminalStart") }),
      codeLine(practiceStartCommand(practice.id))) : null);
  panel.querySelector('[aria-current="step"]')?.scrollIntoView({ block: "nearest" });
}

function renderTutorials(main) {
  const id = S.view.startsWith("training:") ? S.view.slice("training:".length) : null;
  const card = h("section", { class: "card tutorial-card", "aria-labelledby": "tutorial-heading" });
  const status = h("p", { role: "alert", class: "tutorial-status" });
  const tools = h("div", { class: "training-library" });
  const input = h("input", { type: "file", accept: ".podbag", hidden: true });
  const install = button("import", t("tutorial.install"), () => input.click(), "primary");
  const title = trainingEntries().find(entry => entry.id === id)?.title;
  put(main, h("header", { class: "toolbar training-toolbar" }, h("h1", { title, text: title || t("nav.tutorials") }),
    h("div", { class: "tools" }, install, input)),
    h("div", { class: "content tutorial" }, status, tools));
  let installed = savedJsonLesson();

  function show(symbol, title, ...body) {
    const heading = h("h2", { id: "tutorial-heading", tabindex: "-1", text: title });
    const moveFocus = card.isConnected && card.hasChildNodes();
    status.textContent = "";
    put(card, h("div", { class: "tutorial-title" }, icon(symbol), heading), body);
    if (moveFocus) heading.focus();
  }
  function save(record) {
    try {
      if (record) localStorage.setItem(TUTORIAL_KEY, JSON.stringify(record));
      else localStorage.removeItem(TUTORIAL_KEY);
      installed = record;
      updateTrainingNav();
      return true;
    } catch {
      status.textContent = t("tutorial.saveError");
      return false;
    }
  }
  async function toolsAction(action, pack = null, moduleId = null) {
    try {
      const records = await API.volumeToolsLab(action, pack, moduleId);
      if (!main.contains(install)) return false;
      setInstalledPractice(records);
      renderNav();
      renderLibrary();
      return true;
    } catch (error) {
      if (main.contains(install)) status.textContent = String(error);
      return false;
    }
  }
  function renderLibrary() {
    const records = id ? S.trainingRecords.filter(r => r.id === id) : S.trainingRecords;
    const entries = records.map(record => {
      const lesson = trainingWords(record.package);
      const practice = practiceFor(record);
      const detail = id && practice;
      const progress = detail ? h("div", { class: "training-progress" })
        : h("p", { class: "tutorial-progress", role: "status",
          text: t(record.completed ? "tutorial.complete" : "tutorial.installed") });
      const announcement = detail ? h("p", { class: "visually-hidden", role: "status" }) : null;
      const heading = id ? null : h("h2", {}, h("button", {
        type: "button", class: "link", text: titleOf(lesson), onclick: () => go(trainingView(record.id)),
      }));
      return {record, practice, progress, announcement, node: h("section", { class: "card tutorial-card" },
        detail ? h("h2", { text: t("training.progress") }) : heading,
        announcement,
        progress,
        id && !practice ? h("p", { class: "lab-guide", text: lesson.guide }) : null,
        id && !practice ? codeLine(practiceStartCommand(record.id)) : null,
        h("div", { class: "buttons-row" },
          practice ? trainingGuideButton(practice.id, "primary") : null,
          button("remove", t("tutorial.uninstall"), async () => {
            if (await confirmDialog(t("tutorial.removeNote"), t("tutorial.uninstall"))) {
              if (await toolsAction("remove", null, record.id) && id) go("tutorials");
            }
          })))};
    });
    const jsonOverview = !id && installed && h("section", { class: "card tutorial-card" },
      h("h2", {}, h("button", {
        type: "button", class: "link", text: titleOf(trainingWords(installed.pack)), onclick: () => go(trainingView(JSON_LESSON)),
      })),
      h("p", { class: "tutorial-progress", text: t(installed.complete ? "tutorial.complete" : "tutorial.installed") }));
    const jsonDetail = id === JSON_LESSON && installed ? card : null;
    put(tools, entries.map(item => item.node), jsonOverview, jsonDetail,
      !entries.length && !jsonOverview && !jsonDetail ? h("p", { class: "nothing", text: t("tutorial.empty") }) : null);
    for (const {record, practice, progress, announcement} of entries) {
      if (!practice) continue;
      watchPractice(progress, practice, null, { completed: record.completed, onStage: (stage, command) => {
        if (id) {
          renderPracticeProgress(progress, announcement, practice, stage, command);
          return;
        }
        const current = stage === "restart" ? "stop" : stage;
        progress.textContent = stage === "complete" ? t("tutorial.complete")
          : stage === "start" ? t("tutorial.installed")
          : t("practice.current") + " · " + t(practice.namespace + current);
      }});
    }
  }
  input.onchange = async () => {
    const file = input.files[0];
    if (!file) return;
    install.disabled = true;
    status.textContent = "";
    try {
      if (file.size > TUTORIAL_LIMIT) throw Error();
      const pack = JSON.parse(await file.text());
      const newId = TRAINING_FORMATS.get(pack?.format);
      if (newId ? !pack.translations?.en?.title : !validTutorial(pack)) throw Error();
      if (!main.contains(install)) return;
      const text = t("tutorial.installConfirm", { title: titleOf(trainingWords(pack)), file: file.name });
      if (!await confirmDialog(text, t("tutorial.install"), { title: t("tutorial.install"), style: "primary" })) return;
      if (!main.contains(install)) return;
      if (newId ? await toolsAction("install", JSON.stringify(pack)) : save({ pack, complete: installed?.complete || false })) {
        await go(trainingView(newId || JSON_LESSON));
      }
    } catch {
      if (main.contains(install)) status.textContent = t("tutorial.invalid");
    } finally {
      input.value = "";
      install.disabled = false;
    }
  };
  function overview() {
    const lesson = trainingWords(installed.pack);
    show("tutorials", titleOf(lesson), h("p", { class: "tutorial-progress",
      text: t(installed.complete ? "tutorial.complete" : "tutorial.installed") }),
      h("p", { text: lesson.goal }),
      h("div", { class: "buttons-row" },
        button("start", t("lab.start"), () => run("start"), "primary"),
        button("remove", t("tutorial.uninstall"), async () => {
          if (await confirmDialog(t("tutorial.removeNote"), t("tutorial.uninstall")) && save(null)) go("tutorials");
        })));
  }

  async function run(action, value) {
    if (card.inert) return;
    card.inert = true;
    try {
      const state = await API.tutorialLab(action, value);
      if (action === "request" && state.response && !installed.complete) {
        if (!save({ ...installed, complete: true })) return;
      }
      card.inert = false;
      if (card.isConnected) renderLab(state);
    } catch (error) {
      status.textContent = String(error);
    } finally {
      card.inert = false;
    }
  }
  function openLab() {
    overview();
    return run("status");
  }

  function renderLab(state, editing = false) {
    if (!state.active) return overview();
    const { pack } = installed;
    const lesson = trainingWords(pack);
    const end = button("stop", t("lab.end"), async () => {
      if (await confirmDialog(t("lab.endNote"), t("lab.end"))) run("end");
    });
    const actions = (...buttons) => h("div", { class: "buttons-row" }, buttons, end);
    const code = (value) => h("pre", { class: "lab-output", text: value });
    const file = state.volume + "/response.json";
    if (!state.volume) {
      const name = h("input", { id: "lab-volume", type: "text", required: true, placeholder: "my-api",
        autocapitalize: "off", spellcheck: "false", maxlength: 64 });
      const create = button("volumes", t("lab.create"), null, "primary");
      create.type = "submit";
      show("volumes", t("lab.volumeStep"), h("p", { text: lesson.create }),
        h("form", { onsubmit: (e) => { e.preventDefault(); run("create", name.value.trim()); } },
          h("label", { class: "lab-label", for: name.id, text: t("lab.name") }), name, actions(create)));
    } else if (state.source === null || editing) {
      const source = h("textarea", { id: "lab-source", rows: 5, spellcheck: "false",
        autocomplete: "off", autocapitalize: "off", "aria-label": file });
      source.value = state.source ?? pack.starter;
      show("logs", t("lab.fileStep"), h("p", { text: lesson.write }),
        h("label", { for: source.id, class: "lab-label mono", text: file }), source,
        actions(button("check", t("lab.save"), () => run("save", source.value), "primary")));
    } else if (!state.url) {
      show("engine", t("lab.serveStep"), h("p", { text: lesson.serve }),
        h("p", { class: "lab-label mono", text: file }), code(state.source),
        actions(button("start", t("lab.serve"), () => run("serve"), "primary"),
          button("rename", t("lab.edit"), () => renderLab(state, true))));
    } else {
      show(state.response ? "check" : "networks", t(state.response ? "lab.responseStep" : "lab.requestStep"),
        h("p", { text: state.response ? t("lab.resultNote") : lesson.request }),
        codeLine(state.url), state.response && code("HTTP " + state.response.status + "\n" + state.response.body),
        actions(button("forward", t("lab.send"), () => run("request"), "primary"),
          button("rename", t("lab.edit"), () => renderLab(state, true))));
    }
  }
  renderLibrary();
  if (id === JSON_LESSON && installed) openLab();
  else return toolsAction("status");
}
