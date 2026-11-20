// A fake Tauri API for preview.html: the window with sample data, without the app or an
// engine. URL parameters: view, lang (en, ko, …), theme (light, dark).
"use strict";

(() => {
  const params = new URLSearchParams(location.search);
  const view = params.get("view") || "containers";
  const now = Math.floor(Date.now() / 1000);
  const listeners = {};
  const emit = (name, payload) => (listeners[["changed", "live", "logs", "pull-progress", "engine-changed", "vm-status"].includes(name) ? `${name}-${token}` : name] || []).forEach((fn) => fn({ payload }));

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

  const kube = {
    pods: [
      { metadata: { name: "web", namespace: "default" }, spec: { containers: [{ name: "nginx" }], nodeName: "worker-1" }, status: { phase: "Running", containerStatuses: [{ name: "nginx", ready: true }] } },
      { metadata: { name: "web", namespace: "staging" }, spec: { containers: [{ name: "nginx" }] }, status: { phase: "Pending" } },
    ],
    deployments: [
      { metadata: { name: "frontend", namespace: "default" }, spec: { replicas: 3, template: { spec: { containers: [{ image: "nginx:alpine" }] } } }, status: { readyReplicas: 3 } },
    ],
    claims: [
      { metadata: { name: "postgres-data", namespace: "default" }, spec: { storageClassName: "standard" }, status: { phase: "Bound", capacity: { storage: "20Gi" } } },
    ],
    services: [
      { metadata: { name: "frontend", namespace: "default" }, spec: { type: "NodePort", clusterIP: "10.43.0.80", ports: [{ port: 80, nodePort: 30080, protocol: "TCP" }] } },
    ],
  };
  const docker = view.startsWith("docker");
  const engine = {
    platform: "macos",
    engine: docker ? "docker" : "podbag",
    engineName: docker ? "Docker" : "podbag",
    shell: true,
    endpoint: "unix:///Users/me/.podbag/run/docker.sock",
    reachable: !["offline", "vm"].includes(view),
    vm: view === "vm" ? "stopped" : "running",
    version: "podbag 0.1.0 (API 1.47)",
    error: view === "vm" ? "vm-stopped" : view === "offline" ? "cannot connect to unix:///Users/me/.podbag/run/docker.sock: No such file or directory" : null,
  };
  if (docker) {
    engine.endpoint = "unix:///Users/me/.docker/run/docker.sock";
    engine.vm = null;
    engine.version = "Docker Desktop 4.50.0 29.0.1 (API 1.52)";
  }
  if (view === "docker-down") {
    engine.reachable = false;
    engine.version = null;
    engine.error = "Docker is not running. Start Docker Desktop.";
  }
  if (view === "offline") { engine.platform = "linux"; engine.vm = null; engine.endpoint = "unix:///run/user/1000/podbag/docker.sock"; }

  const logLines = [
    "/docker-entrypoint.sh: Configuration complete; ready for start up",
    "2026/11/20 03:12:01 [notice] 1#1: using the \"epoll\" event method",
    "2026/11/20 03:12:01 [notice] 1#1: nginx/1.27.3",
    "2026/11/20 03:12:01 [notice] 1#1: start worker processes",
    ...Array.from({ length: 24 }, (_, i) => `10.77.0.1 - - [20/Nov/2026:03:${String(14 + i).padStart(2, "0")}:07 +0000] "GET /api/items?page=${i + 1} HTTP/1.1" 200 ${1200 + i * 37} "-" "curl/8.11.0"`),
  ];

  // podbag vm status --json
  const vmStatus = view === "vm" ? {
    platform: "macos", state: "stopped", resourcesEditable: true, inUse: null,
    next: { cpus: 4, memoryMiB: 4096, idleSeconds: 300 }, saved: { cpus: 4, memoryMiB: 4096, idleSeconds: null },
    overriddenBy: [], pending: false,
    limits: { cpusMin: 1, cpusMax: 10, memoryMinMiB: 1024, memoryMaxMiB: 20480, macMemoryMiB: 24576 },
    uptimeSeconds: null, containers: null, cpuPercent: null, memoryUsedMiB: null, memoryTotalMiB: null,
    disk: { path: "/Users/me/.podbag/vm/disk.img", allocatedBytes: 3_400_000_000, sizeBytes: 68_719_476_736 },
  } : {
    platform: "macos", state: "running", resourcesEditable: true,
    inUse: { cpus: 4, memoryMiB: 4096, idleSeconds: 300 },
    next: { cpus: 4, memoryMiB: 6144, idleSeconds: 300 }, saved: { cpus: null, memoryMiB: 6144, idleSeconds: null },
    overriddenBy: view === "settings-vm" ? ["PODBAG_VM_CPUS"] : [], pending: true,
    limits: { cpusMin: 1, cpusMax: 10, memoryMinMiB: 1024, memoryMaxMiB: 20480, macMemoryMiB: 24576 },
    uptimeSeconds: 7560, containers: 3, cpuPercent: 12.5, memoryUsedMiB: 1350, memoryTotalMiB: 3900,
    disk: { path: "/Users/me/.podbag/vm/disk.img", allocatedBytes: 5_800_000_000, sizeBytes: 68_719_476_736 },
  };
  const update = view.startsWith("settings") || view === "update-admin" || view === "collapsed"
    ? { engine: "0.1.0", app: "0.1.0", latest: "0.2.0", newer: true, checked: now - 3600, error: null }
    : { engine: "0.1.0", app: "0.1.0", latest: "0.1.0", newer: false, checked: now - 3600, error: null };

  const hub = [
    { name: "nginx", description: "Official build of Nginx.", star_count: 21034, is_official: true },
    { name: "nginxinc/nginx-unprivileged", description: "Unprivileged NGINX Dockerfiles", star_count: 172, is_official: false },
    { name: "nginx/nginx-ingress", description: "NGINX and NGINX Plus Ingress Controllers for Kubernetes", star_count: 109, is_official: false },
    { name: "bitnami/nginx", description: "Bitnami container image for NGINX", star_count: 197, is_official: false },
  ];
  const registries = [
    { id: "dockerhub", label: "Docker Hub", host: "docker.io", needsHost: false, account: null, hint: null },
    { id: "github", label: "GitHub", host: "ghcr.io", needsHost: false, account: "octocat", hint: null },
    { id: "ecr", hostExample: "123456789012.dkr.ecr.us-east-1.amazonaws.com", label: "Amazon ECR", host: "", needsHost: true, account: null, hint: "aws ecr get-login-password --region <region> | docker login --username AWS --password-stdin <account>.dkr.ecr.<region>.amazonaws.com" },
    { id: "quay", label: "Quay.io", host: "quay.io", needsHost: false, account: null, hint: null },
    { id: "gar", hostExample: "us-docker.pkg.dev", label: "Google Artifact Registry", host: "", needsHost: true, account: null, hint: "gcloud auth configure-docker <region>-docker.pkg.dev" },
    { id: "gitlab", label: "GitLab", host: "registry.gitlab.com", needsHost: false, account: null, hint: null },
    { id: "acr", hostExample: "contoso.azurecr.io", label: "Azure Container Registry", host: "", needsHost: true, account: null, hint: "az acr login --name <registry-name>" },
    { id: "ecr-public", label: "Amazon ECR Public", host: "public.ecr.aws", hostExample: "public.ecr.aws", needsHost: false },
    { id: "mcr", label: "Microsoft Artifact Registry", host: "mcr.microsoft.com", hostExample: "mcr.microsoft.com", needsHost: false },
    { id: "dhi", label: "Docker Hardened Images", host: "dhi.io", hostExample: "dhi.io", needsHost: false },
    { id: "gcr", label: "Google Artifact Registry (gcr.io)", host: "", hostExample: "gcr.io", needsHost: true },
    { id: "digitalocean", label: "DigitalOcean Container Registry", host: "registry.digitalocean.com", hostExample: "registry.digitalocean.com", needsHost: false },
    { id: "ocir", label: "Oracle Cloud Infrastructure Registry", host: "", hostExample: "iad.ocir.io", needsHost: true },
    { id: "oracle", label: "Oracle Container Registry", host: "container-registry.oracle.com", hostExample: "container-registry.oracle.com", needsHost: false },
    { id: "ibm", label: "IBM Cloud Container Registry", host: "", hostExample: "us.icr.io", needsHost: true },
    { id: "alibaba", label: "Alibaba Cloud Container Registry", host: "", hostExample: "crpi-example.cn-hangzhou.personal.cr.aliyuncs.com", needsHost: true },
    { id: "tencent", label: "Tencent Container Registry", host: "", hostExample: "example.tencentcloudcr.com", needsHost: true },
    { id: "huawei", label: "Huawei Cloud SWR", host: "", hostExample: "swr.cn-north-4.myhuaweicloud.com", needsHost: true },
    { id: "scaleway", label: "Scaleway Container Registry", host: "", hostExample: "rg.fr-par.scw.cloud", needsHost: true },
    { id: "ovh", label: "OVHcloud Managed Private Registry", host: "", hostExample: "example.container-registry.ovh.net", needsHost: true },
    { id: "redhat", label: "Red Hat Container Registry", host: "", hostExample: "registry.redhat.io", needsHost: true },
    { id: "suse", label: "SUSE Registry", host: "registry.suse.com", hostExample: "registry.suse.com", needsHost: false },
    { id: "kubernetes", label: "Kubernetes Registry", host: "registry.k8s.io", hostExample: "registry.k8s.io", needsHost: false },
    { id: "ngc", label: "NVIDIA NGC", host: "nvcr.io", hostExample: "nvcr.io", needsHost: false },
    { id: "chainguard", label: "Chainguard Registry", host: "cgr.dev", hostExample: "cgr.dev", needsHost: false },
    { id: "cloudsmith", label: "Cloudsmith", host: "docker.cloudsmith.io", hostExample: "docker.cloudsmith.io", needsHost: false },
    { id: "jfrog", label: "JFrog Artifactory", host: "", hostExample: "example.jfrog.io", needsHost: true },
    { id: "harbor", label: "Harbor", host: "", hostExample: "harbor.example.com", needsHost: true },
    { id: "nexus", label: "Sonatype Nexus Repository", host: "", hostExample: "nexus.example.com:5000", needsHost: true },
    { id: "gitlab-self", label: "GitLab Self-Managed", host: "", hostExample: "registry.example.com", needsHost: true },
    { id: "custom", label: "Other registry", host: "", needsHost: true, account: null, hint: null },
  ];
  const elsewhere = ["banner", "settings-docker", "pull", "pull-locations"].includes(view);
  const dockerCmd = elsewhere
    ? { installed: true, name: "Docker Desktop", isPodbag: false, fromEnv: false, context: "desktop-linux", previous: null }
    : { installed: true, name: "podbag", isPodbag: true, fromEnv: false, context: "podbag", previous: "desktop-linux" };

  const selected = sessionStorage.getItem("preview-engine") || params.get("engine") || engine.engine;
  const token = Number(sessionStorage.getItem("preview-token") || 1);
  if (selected !== engine.engine) {
    Object.assign(engine, { engine: selected, engineName: selected === "kiri" ? "Kiri" : selected,
      endpoint: selected === "kiri" ? "unix:///Users/me/.kiri/run/docker.sock" : selected, vm: null, version: selected });
  }
  let launchAtLogin = false;
  const commands = {
    "plugin:autostart|is_enabled": () => launchAtLogin,
    "plugin:autostart|enable": () => { launchAtLogin = true; },
    "plugin:autostart|disable": () => { launchAtLogin = false; },
    connection: () => [token, selected, selected === "podbag"],
    contexts: () => ["desktop-linux", "orbstack"],
    kube_contexts: () => ["dev", "staging"],
    migration_sources: () => ["docker", "podman", "kiri", "podbag"],
    kube_counts: ({ allNamespaces }) => Object.fromEntries(Object.entries(kube).map(([kind,items]) =>
      [kind,items.filter(i=>allNamespaces||i.metadata.namespace==="default").length])),
    migration_list: ({ kind }) => lists[kind],
    migrate: async ({ request, updates }) => {
      for (const phase of ["prepare", request.kind === "images" ? "image" : "files", "verify"]) {
        updates.onmessage(phase);
        await new Promise(resolve => setTimeout(resolve, 180));
      }
    },
    create_volume: ({ name }) => { volumes.push({ Name: name, Driver: "local" }); },
    engine: () => engine,
    list: ({ kind }) => lists[kind],
    kube_list: ({ kind, allNamespaces, context }) => ({
      context: context || "dev", namespace: "default",
      items: kube[kind].filter((item) => allNamespaces || item.metadata.namespace === "default"),
    }),
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
    vm: ({ action }) => {
      engine.vm = action !== "stop" ? "running" : "stopped";
      engine.reachable = action !== "stop";
      vmStatus.state = engine.vm;
      emit("engine-changed", action);
    },
    vm_status: () => (docker || engine.platform === "linux" ? null : vmStatus),
    vm_set: () => "Saved.",
    set_engine: ({ engine }) => {
      sessionStorage.setItem("preview-engine", engine);
      sessionStorage.setItem("preview-token", String(token + 1));
    },
    updates: () => update,
    upgrade: () => ({ ok: false, code: 77, needsAdmin: true, command: "sudo /usr/local/bin/podbag upgrade", output: "podbag: cannot write to /usr/local/bin: it needs administrator rights" }),
    open_terminal: () => null,
    search: ({ term }) => ({ supported: true, error: null, results: hub.filter((r) => r.name.includes(term)) }),
    registries: ({ hosts }) => {
      const saved = Object.fromEntries(hosts);
      return registries.map((loc) => {
        const host = saved[loc.id] || loc.host;
        const hint = loc.id === "acr" && host ? `az acr login --name ${host.split(/[.-]/)[0]}` : loc.hint;
        return { ...loc, host, hint };
      });
    },
    registry_login: () => null,
    docker_command: () => dockerCmd,
    docker_switch: () => ({ ...dockerCmd, name: "podbag", isPodbag: true }),
    docker_images: () => [
      { name: "nginx:alpine", size: 52_400_000, registry: "dockerhub" },
      { name: "mysql:8.4", size: 603_000_000, registry: "dockerhub" },
      { name: "ghcr.io/example/worker:1.4", size: 183_000_000, registry: "github" },
      { name: "demo-registry.cn-shanghai.cr.aliyuncs.com/team/api:2", size: 76_000_000, registry: "alibaba" },
      { name: "contoso.azurecr.io/team/api:2", size: 84_000_000, registry: "acr" },
      { name: "registry.example.com/team/worker:1.4.2", size: 183_000_000, registry: "custom" },
    ],
    import_images: () => null,
    get_settings: () => ({
      language: params.get("lang") || "en", theme: params.get("theme") || "system",
      sidebarWidth: 216, sidebarCollapsed: view === "collapsed",
    }),
    set_settings: () => null,
  };

  window.__TAURI__ = {
    core: { Channel: class {}, invoke: async (cmd, args) => commands[cmd](args || {}) },
    event: { listen: async (name, fn) => { (listeners[name] ||= []).push(fn); return () => {}; } },
  };

  // After the app starts: show the view asked for.
  window.addEventListener("podbag-preview", async () => {
    const app = window.podbag;
    await app.ready;
    const scenes = {
      pods: () => app.go("kubernetes"),
      claims: () => app.go("kubernetes", "claims"),
      services: () => app.go("kubernetes", "services"),
      images: () => app.go("images"),
      volumes: () => app.go("volumes"),
      networks: () => app.go("networks"),
      settings: () => app.go("settings"),
      "settings-vm": () => app.openSettingsSection("vm-settings"),
      "settings-updates": app.openUpdates,
      "update-admin": async () => { await app.openUpdates(); await app.runUpgrade(); },
      "docker-settings": () => app.openSettingsSection("engine-settings"),
      detail: () => app.openDetail("containers", containers[0].Id, "info"),
      logs: () => app.openDetail("containers", containers[0].Id, "logs"),
      inspect: () => app.openDetail("containers", containers[0].Id, "inspect"),
      confirm: () => app.remove("containers", 0),
      pull: async () => { app.go("images"); await app.pull(); app.searchFor("nginx"); await new Promise((r) => setTimeout(r, 400)); },
      "pull-locations": async () => { app.go("images"); await app.pull(); document.querySelector(".loc-button").click(); },
      "pull-alibaba": async () => {
        app.go("images"); await app.pull();
        document.querySelector(".loc-button").click();
        const input = document.querySelector(".loc-search");
        input.value = "Alibaba ACR";
        input.dispatchEvent(new Event("input"));
      },
      "pull-login": async () => { app.go("images"); await app.pull(); app.pullState.location = "quay"; app.pullState.step = "login"; app.render(); },
      banner: () => {},
      "settings-docker": () => app.openSettingsSection("engine-settings"),
      import: async () => { app.go("images"); await app.importFromDocker(); },
      "empty-volumes": () => app.go("volumes"),
    };
    if (view === "empty" && params.get("tab")) app.go(params.get("tab"));
    await (scenes[view] || (() => {}))();
    document.documentElement.dataset.ready = "1";
  });
})();
