// The app's side of the Rust commands (desktop/src/main.rs). Everything goes through
// here, so the page can run on a mocked API (desktop/preview/mock.js).
"use strict";

const API = (() => {
  const tauri = window.__TAURI__;
  const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
  let token;
  const scoped = (cmd, args) => invoke(cmd, { ...args, token });
  const events = new Set(["changed", "live", "logs", "pull-progress", "engine-changed", "vm-status"]);
  const listen = (name, fn) => tauri.event.listen(events.has(name) ? `${name}-${token}` : name, (e) => fn(e.payload));

  return {
    async connection() {
      const [id, name, local] = await invoke("connection");
      token = id;
      return { name, local };
    },
    migrationList: (source, kind) => invoke("migration_list", { source, kind }),
    migrate(request, onProgress) {
      const updates = new tauri.core.Channel();
      updates.onmessage = onProgress;
      return invoke("migrate", { request, updates });
    },
    contexts: () => invoke("contexts"),
    migrationSources: () => invoke("migration_sources"),
    kubeCounts: (context, allNamespaces) => invoke("kube_counts", { context: context || null, allNamespaces }),
    kubeContexts: () => invoke("kube_contexts"),
    createVolume: (name) => scoped("create_volume", { name }),
    engine: (start) => scoped("engine", { start: !!start }),
    list: (kind) => scoped("list", { kind }),
    kubeList: (kind, allNamespaces, context) => invoke("kube_list", { kind, allNamespaces, context: context || null }),
    inspect: (kind, id) => scoped("inspect", { kind, id }),
    act: (id, action) => scoped("act", { id, action }),
    remove: (kind, id, force) => scoped("remove", { kind, id, force: !!force }),
    prune: (kind) => scoped("prune", { kind }),
    rename: (id, name) => scoped("rename", { id, name }),
    containerJson: (id, what) => scoped("container_json", { id, what }),
    features: (sample) => scoped("features", { sample: sample || null }),
    pull: (image, location, host) => scoped("pull", { image, location: location || null, host: host || null }),
    search: (term) => scoped("search", { term }),
    registries: (hosts) => invoke("registries", { hosts }),
    registryLogin: (location, host, user, password) => invoke("registry_login", { location, host, user, password }),
    dockerCommand: () => invoke("docker_command"),
    dockerSwitch: (to) => invoke("docker_switch", { to }),
    dockerImages: (hosts) => invoke("docker_images", { hosts }),
    importImages: (names) => scoped("import_images", { names }),
    logsStart: (id) => scoped("logs_start", { id }),
    logsStop: () => scoped("logs_stop"),
    watch: (on) => scoped("watch", { on }),
    openShell: (id) => scoped("open_shell", { id }),
    vm: (action) => scoped("vm", { action }),
    vmResources: (values) => scoped("vm_resources", { values: values || null }),
    launchAtLogin: () => invoke("plugin:autostart|is_enabled"),
    setLaunchAtLogin: (enabled) => invoke(enabled ? "plugin:autostart|enable" : "plugin:autostart|disable"),
    vmStatus: () => scoped("vm_status"),
    vmSet: (settings) => scoped("vm_set", { settings }),
    setEngine: (engine) => scoped("set_engine", { engine }),
    updates: (check) => invoke("updates", { check }),
    upgrade: () => invoke("upgrade"),
    openTerminal: (what) => invoke("open_terminal", { what }),
    getSettings: () => invoke("get_settings"),
    setSettings: (settings) => invoke("set_settings", { settings }),
    // Events: changed, live, logs, pull-progress, shown, engine-changed, open-updates,
    // open-settings, docker-switching, docker-changed, vm-status.
    on: listen,
  };
})();
