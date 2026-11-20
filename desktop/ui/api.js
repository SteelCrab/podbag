// The app's side of the Rust commands (desktop/src/main.rs). Everything goes through
// here, so the page can run on a mocked API (desktop/preview/mock.js).
"use strict";

const API = (() => {
  const tauri = window.__TAURI__;
  const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
  const listen = (name, fn) => tauri.event.listen(name, (e) => fn(e.payload));

  return {
    engine: (start) => invoke("engine", { start: !!start }),
    list: (kind) => invoke("list", { kind }),
    inspect: (kind, id) => invoke("inspect", { kind, id }),
    act: (id, action) => invoke("act", { id, action }),
    remove: (kind, id, force) => invoke("remove", { kind, id, force: !!force }),
    prune: (kind) => invoke("prune", { kind }),
    rename: (id, name) => invoke("rename", { id, name }),
    containerJson: (id, what) => invoke("container_json", { id, what }),
    features: (sample) => invoke("features", { sample: sample || null }),
    pull: (image) => invoke("pull", { image }),
    logsStart: (id) => invoke("logs_start", { id }),
    logsStop: () => invoke("logs_stop"),
    watch: (on) => invoke("watch", { on }),
    openShell: (id) => invoke("open_shell", { id }),
    vm: (action) => invoke("vm", { action }),
    getSettings: () => invoke("get_settings"),
    setSettings: (settings) => invoke("set_settings", { settings }),
    // Events: changed, live, logs, pull-progress, shown, engine-changed.
    on: listen,
  };
})();
