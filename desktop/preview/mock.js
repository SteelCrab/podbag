// A fake Tauri API for preview.html: the window with sample data, without the app or an
// engine. URL parameters: view, lang (en, ko, …), theme (light, dark).
"use strict";

(() => {
  const params = new URLSearchParams(location.search);
  const view = params.get("view") || "containers";
  const now = Math.floor(Date.now() / 1000);
  const listeners = {};
  const emit = (name, payload) => (listeners[name] || []).forEach((fn) => fn({ payload }));

  const port = (priv, pub) => ({ IP: "0.0.0.0", PrivatePort: priv, PublicPort: pub, Type: "tcp" });
  const containers = [
    { Id: "a1b2c3d4e5f60718", Names: ["/web"], Image: "nginx:alpine", Command: "nginx -g 'daemon off;'", Created: now - 7200, State: "running", Status: "Up 2 hours", Ports: [port(80, 8080)] },
    { Id: "b2c3d4e5f6a70819", Names: ["/db"], Image: "postgres:16-alpine", Command: "docker-entrypoint.sh postgres", Created: now - 86400 * 3, State: "running", Status: "Up 3 days", Ports: [port(5432, 5432)] },
    { Id: "c3d4e5f6a7b8091a", Names: ["/cache"], Image: "redis:7-alpine", Command: "redis-server", Created: now - 86400 * 3, State: "running", Status: "Up 3 days", Ports: [port(6379, 0)] },
    { Id: "d4e5f6a7b8c9102b", Names: ["/worker"], Image: "registry.example.com/team/worker:1.4.2", Command: "/app/worker --queue jobs", Created: now - 3600 * 5, State: "exited", Status: "Exited (1) 4 hours ago", Ports: [] },
    { Id: "e5f6a7b8c9d0213c", Names: ["/migrate"], Image: "myapp:1.4", Command: "./migrate up", Created: now - 86400 * 9, State: "exited", Status: "Exited (0) 9 days ago", Ports: [] },
  ];
  const images = [
    { Id: "sha256:9d1f6a0c3e2b44aa", RepoTags: ["nginx:alpine"], Created: now - 86400 * 12, Size: 52_400_000, Containers: 1 },
    { Id: "sha256:8c2e5b1d7f3a55bb", RepoTags: ["postgres:16-alpine"], Created: now - 86400 * 20, Size: 251_000_000, Containers: 1 },
    { Id: "sha256:7b3d4c2e8a4b66cc", RepoTags: ["redis:7-alpine"], Created: now - 86400 * 30, Size: 41_200_000, Containers: 1 },
    { Id: "sha256:6a4c3d1f9b5c77dd", RepoTags: ["registry.example.com/team/worker:1.4.2"], Created: now - 86400 * 2, Size: 183_000_000, Containers: 1 },
    { Id: "sha256:5f5b2e0a0c6d88ee", RepoTags: ["alpine:3.22"], Created: now - 86400 * 60, Size: 8_520_000, Containers: 0 },
  ];
  const volumes = [
    { Name: "pgdata", Driver: "local", Mountpoint: "/var/lib/podbag/volumes/pgdata/_data", CreatedAt: new Date((now - 86400 * 3) * 1000).toISOString() },
    { Name: "redis-data", Driver: "local", Mountpoint: "/var/lib/podbag/volumes/redis-data/_data", CreatedAt: new Date((now - 86400 * 3) * 1000).toISOString() },
    { Name: "3f2a9c41d7e8b6a5c0f1e2d3b4a59687", Driver: "local", Mountpoint: "/var/lib/podbag/volumes/3f2a9c41d7e8b6a5c0f1e2d3b4a59687/_data", CreatedAt: new Date((now - 86400 * 40) * 1000).toISOString() },
  ];
  const networks = [
    { Name: "bridge", Id: "4e1c0b8a2f6d9e7c", Driver: "bridge", Scope: "local", IPAM: { Config: [{ Subnet: "10.77.0.0/16", Gateway: "10.77.0.1" }] } },
    { Name: "app", Id: "5f2d1c9b3a7e0f8d", Driver: "bridge", Scope: "local", IPAM: { Config: [{ Subnet: "10.77.1.0/24", Gateway: "10.77.1.1" }] } },
    { Name: "host", Id: "6a3e2d0c4b8f1a9e", Driver: "host", Scope: "local", IPAM: { Config: [] } },
    { Name: "none", Id: "7b4f3e1d5c9a2b0f", Driver: "null", Scope: "local", IPAM: { Config: [] } },
  ];
  const lists = { containers, images, volumes, networks };
  if (view === "empty") { lists.containers = []; lists.volumes = []; }

  const engine = {
    platform: "macos",
    endpoint: "unix:///Users/me/.podbag/run/docker.sock",
    reachable: !["offline", "vm"].includes(view),
    vm: view === "vm" ? "stopped" : "running",
    version: "podbag 0.1.0 (API 1.47)",
    error: view === "vm" ? "vm-stopped" : view === "offline" ? "cannot connect to unix:///Users/me/.podbag/run/docker.sock: No such file or directory" : null,
  };
  if (view === "offline") { engine.platform = "linux"; engine.vm = null; engine.endpoint = "unix:///run/user/1000/podbag/docker.sock"; }

  const logLines = [
    "/docker-entrypoint.sh: Configuration complete; ready for start up",
    "2026/11/20 03:12:01 [notice] 1#1: using the \"epoll\" event method",
    "2026/11/20 03:12:01 [notice] 1#1: nginx/1.27.3",
    "2026/11/20 03:12:01 [notice] 1#1: start worker processes",
    ...Array.from({ length: 24 }, (_, i) => `10.77.0.1 - - [20/Nov/2026:03:${String(14 + i).padStart(2, "0")}:07 +0000] "GET /api/items?page=${i + 1} HTTP/1.1" 200 ${1200 + i * 37} "-" "curl/8.11.0"`),
  ];

  const commands = {
    engine: () => engine,
    list: ({ kind }) => lists[kind],
    inspect: ({ kind, id }) => ({ Id: id, Name: "/web", Created: "2026-11-20T01:12:00Z", State: { Status: "running", Running: true, Pid: 4182, StartedAt: "2026-11-20T01:12:01Z" }, Config: { Image: "nginx:alpine", Cmd: ["nginx", "-g", "daemon off;"], Env: ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "NGINX_VERSION=1.27.3"], Tty: false }, HostConfig: { PortBindings: { "80/tcp": [{ HostIp: "", HostPort: "8080" }] }, RestartPolicy: { Name: "no" } }, NetworkSettings: { IPAddress: "10.77.0.2" }, Kind: kind }),
    act: () => null,
    remove: () => null,
    prune: () => ({ count: 2, bytes: 61_000_000 }),
    rename: () => null,
    container_json: () => { throw "not supported"; },
    features: () => ({ stats: false, top: false, pause: false, rename: false }),
    pull: () => new Promise(() => {}),
    logs_start: () => { setTimeout(() => emit("logs", { generation: 1, lines: logLines }), 30); return 1; },
    logs_stop: () => null,
    watch: () => { setTimeout(() => emit("live", true), 10); return null; },
    open_shell: () => null,
    vm: () => null,
    get_settings: () => ({ language: params.get("lang") || "en", theme: params.get("theme") || "system" }),
    set_settings: () => null,
  };

  window.__TAURI__ = {
    core: { invoke: async (cmd, args) => commands[cmd](args || {}) },
    event: { listen: async (name, fn) => { (listeners[name] ||= []).push(fn); return () => {}; } },
  };

  // After the app starts: show the view asked for.
  window.addEventListener("podbag-preview", async () => {
    const app = window.podbag;
    await app.ready;
    const scenes = {
      images: () => app.go("images"),
      volumes: () => app.go("volumes"),
      networks: () => app.go("networks"),
      settings: () => app.go("settings"),
      detail: () => app.openDetail("containers", containers[0].Id, "info"),
      logs: () => app.openDetail("containers", containers[0].Id, "logs"),
      inspect: () => app.openDetail("containers", containers[0].Id, "inspect"),
      confirm: () => app.remove("containers", 0),
      pull: () => { app.go("images"); app.pull(); },
      "empty-volumes": () => app.go("volumes"),
    };
    if (view === "empty" && params.get("tab")) app.go(params.get("tab"));
    (scenes[view] || (() => {}))();
    document.documentElement.dataset.ready = "1";
  });
})();
