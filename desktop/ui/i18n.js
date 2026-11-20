// The string tables (i18n/<language>.json): English, then the chosen language over it.
"use strict";

const I18N = (() => {
  const LANGUAGES = ["en", "ko", "ja", "zh-CN", "fil", "es", "vi"];
  let english = {};
  let table = {};
  let lang = "en";

  // One of LANGUAGES for a locale tag (ko-KR, zh-Hans-CN, tl), or null.
  function match(tag) {
    const base = String(tag || "").toLowerCase().replace("_", "-").split("-")[0];
    if (base === "zh") return "zh-CN";
    if (base === "fil" || base === "tl") return "fil";
    return LANGUAGES.find((l) => l === base) || null;
  }

  // The setting, or the system's language for "system".
  function resolve(setting) {
    if (LANGUAGES.includes(setting)) return setting;
    for (const tag of navigator.languages || [navigator.language]) {
      const m = match(tag);
      if (m) return m;
    }
    return "en";
  }

  async function fetchTable(name) {
    const res = await fetch(`i18n/${name}.json`);
    if (!res.ok) throw new Error(`i18n/${name}.json: ${res.status}`);
    return res.json();
  }

  async function load(setting) {
    lang = resolve(setting);
    if (!Object.keys(english).length) english = await fetchTable("en");
    table = lang === "en" ? english : await fetchTable(lang).catch(() => english);
    document.documentElement.lang = lang;
    return lang;
  }

  // The text for key, with {name} parts filled in from vars.
  function t(key, vars) {
    let s = table[key] ?? english[key] ?? key;
    if (vars) {
      for (const [k, v] of Object.entries(vars)) s = s.replaceAll(`{${k}}`, String(v));
    }
    return s;
  }

  // Each language's own name, for the settings.
  async function names() {
    const out = {};
    for (const l of LANGUAGES) {
      try {
        out[l] = (await fetchTable(l))["language.name"] || l;
      } catch {
        out[l] = l;
      }
    }
    return out;
  }

  return { LANGUAGES, load, t, names, get lang() { return lang; } };
})();
