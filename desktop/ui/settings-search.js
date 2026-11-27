// Search the controls that Settings rendered. Closed sections can be searched too.
"use strict";

function searchableSetting(key, keywords) {
  return { "data-setting-search": key, "data-setting-keywords": keywords };
}

function settingsSearch(sections, value = "") {
  const label = t("settings.search.label");
  const input = h("input", {
    type: "search", id: "settings-search", class: "search",
    placeholder: label, "aria-label": label, "aria-controls": "settings-search-results",
    autocomplete: "off", spellcheck: "false", oninput: showResults,
  });
  input.value = value;
  const clear = iconButton("close", t("settings.search.clear"), () => clearSearch(), "settings-search-clear");
  const status = h("p", { role: "status" });
  const results = h("section", { id: "settings-search-results", class: "card settings-search-results", "aria-label": label },
    status, h("ul", {}));
  const list = results.querySelector("ul");
  const wrapper = h("div", { class: "settings-search", role: "search", "aria-label": label, onkeydown: keyDown },
    h("div", { class: "search-box" }, icon("search"), input, clear), results);
  const selector = "[data-setting-search]";
  let lastResults;

  function fold(text) {
    return text.normalize("NFKD").replace(/\p{Diacritic}/gu, "").toLocaleLowerCase(I18N.lang).trim();
  }

  function sectionOf(control) {
    return control.closest("details")?.querySelector(":scope > summary");
  }

  function showResults() {
    const query = fold(input.value);
    const searching = query.length > 0;
    clear.hidden = !input.value;
    results.hidden = !searching;
    if (sections.hidden !== searching) sections.hidden = searching;
    const matches = [];
    if (searching) {
      const words = query.split(/\s+/);
      for (const control of sections.querySelectorAll(selector)) {
        const hidden = control.closest("[hidden]");
        if (hidden && hidden !== sections) continue;
        const key = control.dataset.settingSearch;
        const text = t(key);
        const parent = sectionOf(control);
        const context = parent && parent !== control ? t(parent.dataset.settingSearch) : "";
        const aliases = control.dataset.settingKeywords?.split(" ").map((term) => t(term)) || [];
        const haystack = fold([text, context, ...aliases].join(" "));
        if (!words.every((word) => haystack.includes(word))) continue;
        const normalized = fold(text);
        const rank = normalized === query ? 0 : normalized.startsWith(query) ? 1 : normalized.includes(query) ? 2 : 3;
        matches.push({ key, text, context, rank });
      }
      matches.sort((a, b) => a.rank - b.rank);
    }

    // VM polling and update checks replace parts of Settings. Keep the result buttons
    // and their focus when these changes haven't changed the visible results.
    const signature = JSON.stringify([searching, matches]);
    if (lastResults === signature) return;
    lastResults = signature;
    status.textContent = searching && !matches.length ? t("settings.search.noResults") : "";
    list.hidden = !matches.length;
    const focused = document.activeElement?.dataset.settingResult;
    put(list, matches.map(({ key, text, context }) =>
      h("li", {}, h("button", {
        type: "button", "data-setting-result": key, onclick: () => revealSetting(key),
      }, h("span", {}, text, context ? h("small", { text: context }) : null), icon("forward")))));
    if (focused) {
      const next = [...list.querySelectorAll("button")].find((button) => button.dataset.settingResult === focused);
      (next || input).focus();
    }
  }

  function revealSetting(key) {
    const target = [...sections.querySelectorAll(selector)].find((control) => control.dataset.settingSearch === key);
    if (!target) return;
    clearSearch(false);
    for (let section = target.closest("details"); section; section = section.parentElement.closest("details")) {
      section.open = true;
    }
    const controls = "summary, input:not(:disabled), select:not(:disabled), button:not(:disabled)";
    const focus = target.matches(controls) ? target : target.querySelector("input[type=radio]:checked")
      || target.querySelector(controls) || sectionOf(target) || target;
    if (!focus.matches(controls)) focus.tabIndex = -1;
    focus.focus({ preventScroll: true });
    focus.scrollIntoView({ block: "nearest" });
  }

  function clearSearch(focus = true) {
    input.value = "";
    showResults();
    if (focus) input.focus();
  }

  function keyDown(event) {
    if (!input.value || event.isComposing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      clearSearch();
      return;
    }
    const buttons = [...list.querySelectorAll("button")];
    if (!buttons.length) return;
    const current = buttons.indexOf(event.target);
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (current === -1 && event.target !== input) return;
      event.preventDefault();
      const next = event.key === "ArrowDown" ? Math.min(current + 1, buttons.length - 1) : current - 1;
      (buttons[next] || input).focus();
    } else if (event.key === "Enter" && event.target === input) {
      event.preventDefault();
      buttons[0].click();
    }
  }

  const observer = new MutationObserver(showResults);
  observer.observe(sections, { subtree: true, childList: true, attributes: true, attributeFilter: ["hidden"] });
  showResults();
  return wrapper;
}
