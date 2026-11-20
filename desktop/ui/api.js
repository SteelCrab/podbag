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
    pull: (image, location, host) => invoke("pull", { image, location: location || null, host: host || null }),
    search: (term) => invoke("search", { term }),
    registries: (hosts) => invoke("registries", { hosts }),
    registryLogin: (location, host, user, password) => invoke("registry_login", { location, host, user, password }),
    dockerCommand: () => invoke("docker_command"),
    dockerSwitch: (to) => invoke("docker_switch", { to }),
    dockerImages: () => invoke("docker_images"),
    importImages: (names) => invoke("import_images", { names }),
    logsStart: (id) => invoke("logs_start", { id }),
    logsStop: () => invoke("logs_stop"),
    watch: (on) => invoke("watch", { on }),
    openShell: (id) => invoke("open_shell", { id }),
    vm: (action) => invoke("vm", { action }),
    vmStatus: () => invoke("vm_status"),
    vmSet: (settings) => invoke("vm_set", { settings }),
    setEngine: (engine) => invoke("set_engine", { engine }),
    updates: (check) => invoke("updates", { check }),
    upgrade: () => invoke("upgrade"),
    openTerminal: (what) => invoke("open_terminal", { what }),
    getSettings: () => invoke("get_settings"),
    setSettings: (settings) => invoke("set_settings", { settings }),
    // Events: changed, live, logs, pull-progress, shown, engine-changed, open-updates,
    // open-settings, docker-changed.
    on: listen,
  };
})();
