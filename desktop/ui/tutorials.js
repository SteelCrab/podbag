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

function lessonIntroduction(pack, lesson) {
  const intro = lesson.intro || pack.translations.en.intro;
  if (!intro) return null;
  const animation = pack.animation;
  const video = animation && h("video", {
    controls: true, playsinline: true, preload: "none", "aria-label": intro.title,
    poster: "data:image/webp;base64," + animation.webp, src: "data:video/mp4;base64," + animation.mp4,
  });
  return h("section", { class: "lesson-intro" },
    h("h3", { text: intro.title }), h("p", { text: intro.text }), video);
}

function renderTutorials(main) {
  const card = h("section", { class: "card tutorial-card", "aria-labelledby": "tutorial-heading" });
  const status = h("p", { role: "status", class: "tutorial-status" });
  const tools = h("section", { class: "card tutorial-card", hidden: true });
  put(main, h("header", { class: "toolbar" }, h("h1", { text: t("nav.tutorials") })),
    h("div", { class: "content tutorial" }, tools, card));
  let installed;
  try {
    const saved = JSON.parse(localStorage.getItem(TUTORIAL_KEY));
    if (validTutorial(saved?.pack) && typeof saved.complete === "boolean") installed = saved;
  } catch { /* The installer can replace an unreadable record. */ }

  const words = (pack) => pack.translations[I18N.lang] || pack.translations.en;
  // The first edition of this module included the lesson number in its translated title.
  const titleOf = (lesson) => lesson.title.replace(/^(Lab|실습|実習|实验|Práctica|Bài thực hành) 1 · /u, "$1 · ");
  function show(symbol, title, ...body) {
    const heading = h("h2", { id: "tutorial-heading", tabindex: "-1", text: title });
    const moveFocus = card.isConnected && card.hasChildNodes();
    status.textContent = "";
    put(card, h("div", { class: "tutorial-title" }, icon(symbol), heading), body, status);
    if (moveFocus) heading.focus();
  }
  function save(record) {
    try {
      if (record) localStorage.setItem(TUTORIAL_KEY, JSON.stringify(record));
      else localStorage.removeItem(TUTORIAL_KEY);
      installed = record;
      return true;
    } catch {
      status.textContent = t("tutorial.saveError");
      return false;
    }
  }
  async function toolsAction(action, pack = null) {
    try {
      const record = await API.volumeToolsLab(action, pack);
      if (!tools.isConnected) return;
      tools.hidden = !record;
      const gui = record?.package.format === GUI_VOLUME_FORMAT;
      if (S.practiceInstalled !== gui) { S.practiceInstalled = gui; renderNav(); }
      if (!record) {
        tools.replaceChildren();
        return true;
      }
      const lesson = words(record.package);
      const progress = gui ? h("section", { class: "practice-progress" }) : null;
      put(tools, h("div", { class: "tutorial-title" }, icon("shell"), h("h2", { tabindex: "-1", text: lesson.title })),
        gui ? null : h("p", { class: "tutorial-progress", text: t(record.completed ? "tutorial.complete" : "tutorial.installed") }),
        lessonIntroduction(record.package, lesson),
        gui ? progress : h("p", { class: "lab-guide", text: lesson.guide }), gui ? null : codeLine("podbag lab start"),
        h("div", { class: "buttons-row" },
          button("shell", t("status.terminal"), () => toolsAction("start"), "primary"),
          gui ? button("volumes", t("practice.volumes"), () => go("lab-volumes")) : button("refresh", t("action.refresh"), () => toolsAction("status")),
          button("remove", t("tutorial.uninstall"), async () => {
            if (await confirmDialog(t("tutorial.removeNote"), t("tutorial.uninstall"))) await toolsAction("remove");
          })));
      if (gui) watchPractice(progress, record.completed);
      return true;
    } catch (error) {
      status.textContent = String(error);
      return false;
    }
  }
  function home() { if (installed) return openLab(); installer(); }
  function installer() {
    const guide = I18N.lang === "en" ? "https://podbag.dev/blog/" : `https://podbag.dev/${I18N.lang}/blog/`;
    const input = h("input", { type: "file", accept: ".podbag", hidden: true });
    const select = button("import", t("tutorial.choose"), () => input.click(), "primary");
    input.onchange = async () => {
      const file = input.files[0];
      if (!file) return;
      select.disabled = true;
      try {
        if (file.size > TUTORIAL_LIMIT) throw Error();
        const pack = JSON.parse(await file.text());
        const cliLab = [GUI_VOLUME_FORMAT, "podbag-cli-tui-volumes-v1"].includes(pack?.format);
        if (cliLab ? !pack.translations?.en?.title : !validTutorial(pack)) throw Error();
        if (!input.isConnected) return;
        show("tutorials", titleOf(words(pack)), h("p", { class: "mono", text: file.name }),
          h("p", { text: t("tutorial.installNote") }),
          h("div", { class: "buttons-row" },
            button("pull", t("tutorial.install"), async () => {
              if (cliLab) {
                if (!await toolsAction("install", JSON.stringify(pack))) return;
                await home();
                tools.querySelector("h2")?.focus();
              } else if (save({ pack, complete: installed?.complete || false })) openLab();
            }, "primary"),
            button(null, t("action.cancel"), home)));
      } catch {
        status.textContent = t("tutorial.invalid");
        input.value = "";
      } finally {
        select.disabled = false;
      }
    };
    show("tutorials", t("tutorial.empty"), h("p", { text: t("tutorial.chooseHelp") }), codeLine(guide),
      h("div", { class: "buttons-row" }, input, select, installed && button(null, t("action.cancel"), home)));
  }
  function overview() {
    const lesson = words(installed.pack);
    show("tutorials", titleOf(lesson), h("p", { class: "tutorial-progress",
      text: t(installed.complete ? "tutorial.complete" : "tutorial.installed") }),
      h("p", { text: lesson.goal }),
      h("div", { class: "buttons-row" },
        button("start", t("lab.start"), () => run("start"), "primary"),
        button("import", t("tutorial.install"), installer),
        button("remove", t("tutorial.uninstall"), async () => {
          if (await confirmDialog(t("tutorial.removeNote"), t("tutorial.uninstall")) && save(null)) installer();
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
    const lesson = words(pack);
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
  home();
  toolsAction("status");
}
