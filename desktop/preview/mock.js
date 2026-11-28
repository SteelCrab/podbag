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
    // Unused for a long time, so the engine compressed it: 3.9 MB on disk, 54% smaller.
    { Id: "sha256:5f5b2e0a0c6d88ee", RepoTags: ["alpine:3.22"], Created: now - 86400 * 60, Size: 8_520_000, Containers: 0,
      PodbagCompression: { Size: 3_900_000, OriginalSize: 8_520_000 } },
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
  if (view.startsWith("analysis")) {
    images[0].Id = `sha256:${"1".repeat(64)}`;
    images[1].Id = `sha256:${"2".repeat(64)}`;
  }
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
  // Only the "ocp" context is OpenShift: it alone serves Routes.
  const routes = [
    { kind: "Route", metadata: { name: "shop", namespace: "default" }, spec: { host: "shop.apps.ocp.example.com", path: "/cart", to: { kind: "Service", name: "frontend" }, tls: { termination: "edge" } } },
    { kind: "Route", metadata: { name: "api", namespace: "default" }, spec: { host: "api.apps.ocp.example.com", to: { kind: "Service", name: "api" } } },
    { kind: "Route", metadata: { name: "docs", namespace: "staging" }, spec: { host: "docs-staging.apps.ocp.example.com", to: { kind: "Service", name: "docs" }, tls: { termination: "passthrough" } } },
  ];
  const kubeItems = (kind, context) => {
    if (kind !== "routes") return kube[kind];
    if (context === "ocp") return routes;
    throw 'kubectl (exit status: 1): error: the server doesn\'t have a resource type "routes"';
  };
  const docker = view.startsWith("docker");
  const platform = params.get("platform") || "macos";
  const engine = {
    platform,
    engine: docker ? "docker" : "podbag",
    engineName: docker ? "Docker" : "podbag",
    shell: true,
    endpoint: "unix:///Users/me/.podbag/run/docker.sock",
    reachable: !["offline", "vm"].includes(view),
    vm: platform === "linux" ? null : view === "vm" ? "stopped" : "running",
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
    sharedPaths: { inUse: null, next: ["/Users"] },
    limits: { cpusMin: 1, cpusMax: 10, memoryMinMiB: 1024, memoryMaxMiB: 20480, macMemoryMiB: 24576 },
    uptimeSeconds: null, containers: null, cpuPercent: null, memoryUsedMiB: null, memoryTotalMiB: null,
    disk: { path: "/Users/me/.podbag/vm/disk.img", allocatedBytes: 3_400_000_000, sizeBytes: 68_719_476_736 },
  } : {
    platform: "macos", state: "running", resourcesEditable: true,
    inUse: { cpus: 4, memoryMiB: 4096, idleSeconds: 300 },
    next: { cpus: 4, memoryMiB: 6144, idleSeconds: 300 }, saved: { cpus: null, memoryMiB: 6144, idleSeconds: null },
    overriddenBy: view === "settings-vm" ? ["PODBAG_VM_CPUS"] : [], pending: true,
    sharedPaths: { inUse: ["/Users"], next: ["/Users"] },
    limits: { cpusMin: 1, cpusMax: 10, memoryMinMiB: 1024, memoryMaxMiB: 20480, macMemoryMiB: 24576 },
    uptimeSeconds: 7560, containers: 3, cpuPercent: 12.5, memoryUsedMiB: 1350, memoryTotalMiB: 3900,
    disk: { path: "/Users/me/.podbag/vm/disk.img", allocatedBytes: 5_800_000_000, sizeBytes: 68_719_476_736 },
  };
  const storageStopped = params.get("vmStorage") === "stopped";
  if (storageStopped) {
    engine.vm = "stopped";
    Object.assign(vmStatus, {
      state: "stopped", inUse: null, uptimeSeconds: null, containers: null,
      cpuPercent: null, memoryUsedMiB: null, memoryTotalMiB: null,
    });
  }
  const storageGib = 2 ** 30;
  const vmStorage = {
    path: "/Users/sam/.podbag/disk.img", exists: true, configured: false, editable: storageStopped,
    sizeBytes: 64 * storageGib, filesystemBytes: 64 * storageGib, allocatedBytes: 4 * storageGib,
    growthPending: false, maxSizeGib: 1024,
  };
  if (platform === "windows") Object.assign(vmStatus, { platform, resourcesEditable: false });
  // A packaged app with a separate podbag for the terminal; update-source: a build of its own.
  const packaged = { installer: "macos-app", terminal: "/usr/local/bin/podbag", terminalVersion: "0.1.0", appNewer: true, cliNewer: true };
  const source = { installer: null, terminal: null, terminalVersion: null, appNewer: false, cliNewer: true };
  const update = view.startsWith("settings") || view.startsWith("update-") || view === "collapsed"
    ? { engine: "0.1.0", app: "0.1.0", latest: "0.2.0", newer: true, checked: now - 3600, error: null, ...(view === "update-source" ? source : packaged) }
    : view === "cli-update"
      // The app came from a new package; the terminal's own podbag stayed behind.
      ? { engine: "0.3.1", app: "0.3.1", latest: "0.3.1", newer: true, checked: now - 3600, error: null, ...packaged, terminalVersion: "0.3.0", appNewer: false, terminalOlder: true }
      : { engine: "0.1.0", app: "0.1.0", latest: "0.1.0", newer: false, checked: now - 3600, error: null, ...packaged, appNewer: false, cliNewer: false };
  // What a terminal runs for podbag: nothing in the views that show Install.
  let terminalCommand = view === "cli-notice" || view === "settings-cli"
    ? { state: "missing", path: null, target: "/usr/local/bin/podbag", problem: null }
    : { state: "other", path: "/usr/local/bin/podbag", target: "/usr/local/bin/podbag", problem: null };

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
  const migrationImages = [
    { Id: "sha256:103afa055a409b34", RepoTags: ["example/api:1.5", "registry.gitlab.com/example/api:1.5"] },
    { Id: "sha256:158bb8104d657e3f", RepoTags: ["ghcr.io/example/frontend:2.0"] },
    { Id: "sha256:498bdb036ad35429", RepoTags: ["quay.io/example/proxy:0.8"] },
    { Id: "sha256:8ab61e38dd2ac135", RepoTags: ["<none>:<none>"] },
  ];
  function migrationRegistry(reference, hosts) {
    const host = reference.includes("/") ? reference.split("/")[0] : "";
    const configured = hosts.find(([, address]) => address && address === host)?.[0];
    const registry = registries.find(r => configured ? r.id === configured : r.host && r.host === host)
      || registries.find(r => r.id === (host.includes(".") ? "custom" : "dockerhub"));
    return { id: registry.id, label: registry.label };
  }
  const elsewhere = ["banner", "settings-docker", "pull", "pull-locations"].includes(view);
  const desktopContext = { installed: true, name: "Docker Desktop", isPodbag: false, fromEnv: false, context: "desktop-linux", previous: null };
  const podbagContext = { installed: true, name: "podbag", isPodbag: true, fromEnv: false, context: "podbag", previous: "desktop-linux" };
  let dockerCmd = elsewhere ? desktopContext : podbagContext;

  const selected = sessionStorage.getItem("preview-engine") || params.get("engine") || engine.engine;
  const token = Number(sessionStorage.getItem("preview-token") || 1);
  if (selected !== engine.engine) {
    Object.assign(engine, { engine: selected, engineName: selected === "kiri" ? "Kiri" : selected,
      endpoint: selected === "kiri" ? "unix:///Users/me/.kiri/run/docker.sock" : selected, vm: null, version: selected });
  }
  let launchAtLogin = false;
  const networkSettings = {
    revision: "0",
    preferences: {
      defaultSubnet: "10.77.0.0/16",
      defaultIpv6Subnet: "fd77:706f:6462::/64",
      embeddedDns: true,
      dnsFilter: "none",
    },
    activeSubnet: "10.77.0.0/16",
    activeIpv6Subnet: null,
    overrides: [],
  };
  // podbag system compression --json. URL: compression=idle (already on), compression=broken
  // (a reply without a mode), override (an environment setting keeps the mode as it is),
  // after (seconds an image stays unused; 0: at once), vm-error (vm status fails), busy
  // (another dialog is open when the window first asks for the VM's state).
  const compression = { mode: params.get("compression") === "idle" ? "idle" : "off", after: Number(params.get("after") ?? 604800) };
  let registryProxy = params.get("registryProxy") === "manual"
    ? { mode: "manual", url: "http://proxy.example.com:3128", bypass: "localhost, .example.com" }
    : { mode: "inherited", url: "", bypass: "" };
  const practiceModules = new Map();
  let localCluster = null;
  window.practiceUi = { action: "remove-image", name: "my-busybox:1" };
  window.practiceCalls = [];
  const checklist = {
    apiVersion: 1, id: "example.workspace-checklist", name: "Workspace checklist", version: "1.0.0",
    description: "Review local resources before changing the workspace.", permissions: ["engineSummary"],
    content: [
      { type: "heading", text: "Before you replace a container" },
      { type: "list", items: ["Check which containers use the image.", "Keep the volumes that contain application data."] },
      { type: "code", text: "podbag ps" },
      { type: "engineSummary" },
    ],
  };
  let extensions = view === "extension-view" ? [{ manifest: checklist, enabled: true, engineSummary: true }] : [];
  const synchronized = view === "settings-sync" ? [{ name: "project", source: "/Users/me/projects/app", volume: "podbag-sync-project", conflicts: [], lastError: null }] : [];
  const demoModel = { name: "smollm2:135m", size: 92_078_644, loaded: true };
  const models = view === "models-report" ? [{ ...demoModel }] : [];
  let modelRunner = view === "models-report";
  const builderAvailability = params.get("builders") || "ready";
  const demoBuilders = new Map([
    ["podbag", { driver: "docker", managed: false, status: "running" }],
    ["ci-builder", { driver: "docker-container", managed: true, status: "running" }],
  ]);
  let currentBuilder = "podbag";
  const builderScope = `preview-buildx-${selected}`;
  const getBuilder = (scope, name) => {
    if (scope !== builderScope || !demoBuilders.has(name)) throw Error("Builder is outside this context");
    return demoBuilders.get(name);
  };
  let engineConfigText = "# Local engine settings\nstore_limit = \"10GiB\"\nembedded_dns = true\n";
  const commands = {
    builder_list: () => ({
      availability: builderAvailability, context: selected, scope: builderScope, outsideScope: false,
      builders: builderAvailability === "ready" ? [...demoBuilders].map(([name, builder]) => ({
        name, driver: builder.driver, managed: builder.managed, current: name === currentBuilder,
      })) : [],
    }),
    builder_inspect: ({ scope, name }) => {
      const builder = getBuilder(scope, name);
      return { error: "", nodes: [{ name: builder.managed ? name + "0" : name, endpoint: selected, status: builder.status,
        buildkit: builder.status === "running" ? "v0.33.1" : "", platforms: "linux/arm64, linux/amd64", error: "" }] };
    },
    builder_action: ({ scope, name, action }) => {
      const builder = getBuilder(scope, name);
      if (action !== "select" && !builder.managed) throw Error("Only docker-container builders can be controlled");
      if (action === "select") currentBuilder = name;
      if (action === "start") builder.status = "running";
      if (action === "stop") builder.status = "stopped";
      if (action === "remove") {
        demoBuilders.delete(name);
        if (currentBuilder === name) currentBuilder = "podbag";
      }
    },
    "plugin:autostart|is_enabled": () => launchAtLogin,
    "plugin:autostart|enable": () => { launchAtLogin = true; },
    "plugin:autostart|disable": () => { launchAtLogin = false; },
    registry_proxy: ({ values }) => {
      if (values) {
        (window.previewRegistryProxyWrites ||= []).push(values);
        registryProxy = values.mode === "manual" ? values : { mode: values.mode, url: "", bypass: "" };
      }
      return { ...registryProxy };
    },
    connection: () => [token, selected, selected === "podbag"],
    contexts: () => ["desktop-linux", "orbstack"],
    kube_contexts: () => ["dev", "staging", "ocp", ...(localCluster ? [localCluster.context] : [])],
    local_kube_status: () => localCluster,
    local_kube_create: ({ version, nodes }) => {
      localCluster = { context: "kind-podbag-demo123456", version, nodes, ready: true };
      return localCluster;
    },
    local_kube_reset: () => localCluster,
    local_kube_remove: () => { localCluster = null; },
    kube_openshift: ({ context }) => context === "ocp",
    kube_open_route: () => null,
    migration_sources: () => ["docker", "podman", "kiri", "podbag"],
    kube_counts: ({ allNamespaces, context }) => Object.fromEntries(Object.keys(context === "ocp" ? { ...kube, routes } : kube).map((kind) =>
      [kind, kubeItems(kind, context).filter(i=>allNamespaces||i.metadata.namespace==="default").length])),
    migration_list: ({ kind, hosts = [] }) => {
      if (kind === "images") return [...lists.images, ...migrationImages].map(image => ({
        ...image, MigrationRegistries: Object.fromEntries(image.RepoTags.filter(tag => tag !== "<none>:<none>")
          .map(tag => [tag, migrationRegistry(tag, hosts)])),
      }));
      if (kind === "containers") return lists.containers.map(container => ({
        ...container, MigrationRegistry: migrationRegistry(container.Image, hosts),
      }));
      return lists[kind];
    },
    migrate: async ({ request, updates }) => {
      for (const phase of ["prepare", request.kind === "images" ? "image" : "files", "verify"]) {
        updates.onmessage(phase);
        await new Promise(resolve => setTimeout(resolve, 180));
      }
    },
    create_volume: ({ name }) => { volumes.push({ Name: name, Driver: "local" }); },
    desktop_extensions: ({ request }) => {
      (window.previewExtensionActions ||= []).push(request);
      const { action, id } = request;
      if (action === "install") {
        let manifest;
        try { manifest = JSON.parse(request.source); } catch { throw { kind: "manifest" }; }
        if (extensions.some((item) => item.manifest.id === manifest.id)) throw { kind: "duplicate" };
        extensions.push({ manifest, enabled: false, engineSummary: false });
      } else if (action !== "list") {
        const item = extensions.find((ext) => ext.manifest.id === id);
        if (!item) throw { kind: "notFound" };
        if (action === "setEnabled") {
          item.enabled = request.enabled;
          if (!item.enabled) item.engineSummary = false;
        } else if (action === "setEngineSummary") {
          if (!item.enabled || !item.manifest.permissions.includes("engineSummary")) throw { kind: "permission" };
          item.engineSummary = request.allow;
        } else if (action === "remove") {
          extensions = extensions.filter((ext) => ext.manifest.id !== id);
        }
      }
      return structuredClone(extensions);
    },
    models: ({ request }) => {
      (window.previewModelActions ||= []).push(request);
      const { action, model } = request;
      if (action === "start") {
        if (params.get("ollama") === "missing") throw { kind: "missing" };
        modelRunner = true;
      } else if (action === "stop") {
        modelRunner = false;
        models.forEach((item) => { item.loaded = false; });
      } else if (action === "pull") {
        if (!models.some((item) => item.name === model)) models.push({ ...demoModel, name: model, loaded: false });
      } else if (action === "run" || action === "unload") {
        const selected = models.find((item) => item.name === model);
        if (!selected) throw { kind: "notInstalled" };
        selected.loaded = action === "run";
        if (selected.loaded) return { response: "Blue." };
      }
      return { running: modelRunner, version: modelRunner ? "0.40.17" : "", models: modelRunner ? models.map((item) => ({ ...item })) : [] };
    },
    engine: () => engine,
    engine_config: ({ request }) => {
      if ((request.edit?.text ?? request.text)?.includes("invalid_setting")) throw "line 2: unknown setting invalid_setting";
      if (request.action === "check") return null;
      if (request.action === "save") {
        if (request.edit.original !== engineConfigText) throw "config.toml changed after it was opened";
        engineConfigText = request.edit.text;
      }
      return { path: "/home/podbag/.config/podbag/config.toml", text: engineConfigText };
    },
    list: ({ kind }) => lists[kind],
    kube_list: ({ kind, allNamespaces, context }) => ({
      context: context || "dev", namespace: "default",
      items: kubeItems(kind, context).filter((item) => allNamespaces || item.metadata.namespace === "default"),
    }),
    inspect: ({ kind, id }) => ({ Id: id, Name: "/web", Created: "2026-11-20T01:12:00Z", State: { Status: "running", Running: true, Pid: 4182, StartedAt: "2026-11-20T01:12:01Z" }, Config: { Image: "nginx:alpine", Cmd: ["nginx", "-g", "daemon off;"], Env: ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "NGINX_VERSION=1.27.3"], Tty: false }, HostConfig: { PortBindings: { "80/tcp": [{ HostIp: "", HostPort: "8080" }] }, RestartPolicy: { Name: "no" } }, NetworkSettings: { IPAddress: "10.77.0.2" }, Kind: kind }),
    act: () => null,
    remove: () => null,
    prune: () => ({ count: 2, bytes: 61_000_000 }),
    rename: () => null,
    analyze_image: ({ id, token: requestToken }) => {
      (window.previewScans ||= []).push({ id, token: requestToken });
      if (requestToken !== token) throw "engineChanged";
      if (!images.some((image) => image.Id === id)) throw "imageUnavailable";
      const mode = params.get("scan");
      if (mode === "missing") throw "scannerMissing";
      const target = "Alpine Linux 3.22";
      return {
        imageId: id,
        packages: mode === "empty" ? [] : [
          { target, name: "busybox", version: "1.37.0-r18" },
          { target, name: "libssl3", version: "3.5.1-r0" },
          { target, name: "musl", version: "1.2.5-r10" },
          { target, name: "zlib", version: "1.3.1-r2" },
        ],
        findings: mode === "empty" ? [] : [
          { target, severity: "high", advisory: "DEMO-2026-0001", package: "libssl3", installed: "3.5.1-r0", fixed: "3.5.2-r0" },
          { target, severity: "medium", advisory: "DEMO-2026-0002", package: "zlib", installed: "1.3.1-r2", fixed: "" },
          { target, severity: "low", advisory: "DEMO-2026-0003", package: "busybox", installed: "1.37.0-r18", fixed: "1.37.0-r19" },
        ],
      };
    },
    container_json: () => { throw "not supported"; },
    features: () => ({ stats: false, top: false, pause: false, rename: false }),
    pull: () => new Promise(() => {}),
    // Like the engine: a missing image is pulled first, then the container starts.
    run_container: async ({ spec }) => {
      if (!images.some((i) => i.RepoTags.includes(spec.image))) {
        for (const line of ["Pulling from library/" + spec.image, "Download complete", "Pull complete"]) {
          emit("pull-progress", line);
          await new Promise((resolve) => setTimeout(resolve, 60));
        }
        images.push({ Id: "sha256:4e3d2c1b0a9f8e7d", RepoTags: [spec.image], Created: now, Size: 9_000_000, Containers: 1 });
      }
      const id = "f00dfeed" + String(containers.length).padStart(8, "0");
      const ports = spec.ports.map((p) => ({ IP: "0.0.0.0", PrivatePort: p.container, PublicPort: p.host, Type: p.protocol }));
      lists.containers.unshift({ Id: id, Names: ["/" + (spec.name || "eager_tesla")], Image: spec.image, Command: spec.command.join(" "),
        Created: Math.floor(Date.now() / 1000), State: "running", Status: "Up 1 second", Ports: ports });
      return id;
    },
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
    vm_storage: ({ edit }) => {
      if (!edit) return { ...vmStorage };
      if (!vmStorage.editable) throw "Stop the VM before changing its storage.";
      (window.previewVmStorageWrites ||= []).push(edit);
      const previous = vmStorage.path;
      if (edit.action === "grow") {
        vmStorage.sizeBytes = edit.gib * storageGib;
        vmStorage.growthPending = true;
        return { ...vmStorage, backupPath: "/Users/sam/.podbag/podbag-disk-backup-preview/disk.img" };
      }
      if (edit.action === "move") {
        vmStorage.path = edit.path;
        vmStorage.configured = true;
        return { ...vmStorage, backupPath: previous };
      }
      throw "Unknown storage change";
    },
    vm_status: () => {
      if (params.has("busy") && !document.getElementById("busy-dialog")) {
        const busy = document.createElement("dialog");
        busy.id = "busy-dialog";
        busy.textContent = "Another question";
        document.body.append(busy);
        busy.showModal();
      }
      if (params.has("vm-error")) throw "cannot read the VM's state";
      return docker || engine.platform === "linux" ? null : vmStatus;
    },
    compression: ({ mode }) => {
      if (selected !== "podbag") throw "Image compression requires the podbag engine";
      if (mode && !params.has("override")) compression.mode = mode;
      if (!mode) window.previewCompressionReads = (window.previewCompressionReads || 0) + 1;
      return params.get("compression") === "broken" ? null : { ...compression };
    },
    network_settings: ({ values, expectedRevision }) => {
      if (selected !== "podbag") throw "Network settings require the local podbag engine";
      if (!values && params.has("network-settings-error")) {
        params.delete("network-settings-error");
        throw "Cannot read the network configuration.";
      }
      if (values?.defaultSubnet === "10.79.4.1/24") throw "The IPv4 subnet has host bits set.";
      if (values) {
        if (expectedRevision !== networkSettings.revision) {
          throw "Network settings changed after they were opened.";
        }
        networkSettings.preferences = values;
        networkSettings.revision = String(Number(networkSettings.revision) + 1);
      }
      return structuredClone(networkSettings);
    },
    vm_set: ({ settings }) => {
      if (settings.sharedPaths) vmStatus.sharedPaths.next = settings.sharedPaths;
      if (settings.restart) vmStatus.sharedPaths.inUse = vmStatus.sharedPaths.next;
      return "Saved.";
    },
    vm_sync: ({ action, name, path }) => {
      if (action === "list") return synchronized.map(share => ({ ...share }));
      const existing = synchronized.find(share => share.name === name);
      if (action === "add") {
        if (existing) throw "Share already exists";
        const share = { name, source: path, volume: `podbag-sync-${name}`, conflicts: [], lastError: null };
        synchronized.push(share);
        return share;
      }
      if (!existing) throw "Share not found";
      if (action === "remove") synchronized.splice(synchronized.indexOf(existing), 1);
      return existing;
    },
    set_engine: ({ engine }) => {
      sessionStorage.setItem("preview-engine", engine);
      sessionStorage.setItem("preview-token", String(token + 1));
    },
    updates: () => update,
    upgrade: () => ({ ok: false, code: 77, needsAdmin: true, command: "sudo /usr/local/bin/podbag upgrade", output: "podbag: cannot write to /usr/local/bin: it needs administrator rights" }),
    update_app: () => null,
    open_releases: () => null,
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
    docker_switch: async ({ to }) => {
      emit("docker-switching", to);
      await new Promise((resolve) => setTimeout(resolve, 650));
      dockerCmd = to === "use" ? podbagContext : desktopContext;
      emit("docker-changed", dockerCmd);
      return dockerCmd;
    },
    docker_images: () => [
      { name: "nginx:alpine", size: 52_400_000, registry: "dockerhub" },
      { name: "mysql:8.4", size: 603_000_000, registry: "dockerhub" },
      { name: "ghcr.io/example/worker:1.4", size: 183_000_000, registry: "github" },
      { name: "demo-registry.cn-shanghai.cr.aliyuncs.com/team/api:2", size: 76_000_000, registry: "alibaba" },
      { name: "contoso.azurecr.io/team/api:2", size: 84_000_000, registry: "acr" },
      { name: "registry.example.com/team/worker:1.4.2", size: 183_000_000, registry: "custom" },
    ],
    import_images: () => null,
    terminal_command: () => terminalCommand,
    install_terminal_command: async () => {
      await new Promise((resolve) => setTimeout(resolve, 650));
      terminalCommand = { ...terminalCommand, state: "app", path: terminalCommand.target };
      return terminalCommand;
    },
    take_star_prompt: () => {
      if (view !== "first-launch" || sessionStorage.getItem("preview.starPromptShown")) return false;
      sessionStorage.setItem("preview.starPromptShown", "1");
      return true;
    },
    open_repository: () => null,
    open_training_guide: () => null,
    // practice-tracks: a course session at a module `ui` step. A test sets window.practiceUi to
    // switch the step (window.practiceStage, window.practiceContainers and window.practiceSession
    // change the stage, the containers and the session), and reads window.practiceCalls:
    // [action, session, name] of each request. An inspect of a name other than the volume's
    // shows a network. window.practiceAfterCall runs after a request is recorded (the reply
    // keeps the state from before it).
    practice_volumes: ({ action, session, name, course }) => {
      if (view !== "practice-tracks" || !course) return null;
      const volume = { Name: "data", Driver: "local", Mountpoint: "/var/lib/podbag/volumes/data/_data", CreatedAt: new Date(now * 1000).toISOString() };
      const state = {
        module: "image-basics", session: window.practiceSession ?? "podbag-container-abc", stage: window.practiceStage ?? "removeTag", command: "podbag rmi my-busybox:1",
        containers: window.practiceContainers ?? [{ Id: "c1", Names: ["/web"], Image: "busybox:1.37.0", Command: "httpd -f", Created: now, State: "running", Status: "Up 1 minute", Ports: [] }],
        volumes: [volume], ui: window.practiceUi, text: null, detail: null,
      };
      if (action === "status") return state;
      window.practiceCalls.push([action, session, name]);
      window.practiceAfterCall?.();
      const network = { Name: name, Id: "n1", Driver: "bridge", Scope: "local", IPAM: { Config: [{ Subnet: "10.89.0.0/24", Gateway: "10.89.0.1" }] } };
      return { ...state, text: action === "logs" ? "hello from web\n" : null, detail: action === "inspect" ? (name === volume.Name ? volume : network) : null };
    },
    volume_tools_lab: ({ action, package: file, id }) => {
      if (action === "install") {
        const pack = JSON.parse(file);
        const key = { "podbag-container-basics-v1": "container-basics",
          "podbag-cli-gui-volumes-v1": "volume-basics", "podbag-cli-tui-volumes-v1": "tui-volumes",
          "podbag-web-basics-v1": "web-basics", "podbag-volume-persistence-v1": "volume-persistence",
          "podbag-web-service-lifecycle-v1": "web-service-lifecycle",
          "podbag-web-service-recovery-v1": "web-service-recovery",
          "podbag-container-network-v1": "container-network" }[pack.format];
        practiceModules.set(key, { id: key, package: pack, completed: false });
      }
      if (action === "remove") practiceModules.delete(id);
      // practice-tracks: a course whose steps carry the module default `ui` (the course's port).
      if (view === "practice-tracks" && !practiceModules.size) {
        const words = { title: "Podbag Training · Image basics", goal: "", guide: "" };
        const names = (en) => ({ en });
        practiceModules.set("image-basics", { id: "image-basics", completed: false,
          package: { format: "podbag-image-basics-v1", translations: { en: words } },
          steps: [
            { key: "pullImage", command: "podbag pull busybox:1.37.0", names: names("Pull the image"),
              ui: { action: "pull", name: "busybox:1.37.0" } },
            { key: "removeTag", command: "podbag rmi my-busybox:1", names: names("Remove the tag"),
              ui: { action: "remove-image", name: "my-busybox:old" } },
            { key: "stopWeb", command: "podbag stop web", names: names("Stop the server"),
              ui: { action: "stop", name: "web" } },
            { key: "checkList", command: "podbag ps -a", names: names("Check the list") },
          ] });
      }
      return [...practiceModules.values()].filter(record => action !== "summary" || !id || record.id === id);
    },
    tutorial_lab: ({ action }) => {
      if (action === "status") return { active: false };
      throw "Start the local lab in Podbag Desktop.";
    },
    get_settings: () => ({
      language: params.get("lang") || "en", theme: params.get("theme") || "system",
      sidebarWidth: 216, sidebarCollapsed: view === "collapsed",
      // compression-prompt (or ask in the URL): a new install, or an update from a version
      // without the question.
      compressionPromptSeen: view !== "compression-prompt" && !params.has("ask"),
    }),
    set_settings: ({ settings }) => { window.previewSavedSettings = settings; },
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
      "local-kube": () => {
        app.go("kubernetes");
        document.getElementById("local-kube-open").click();
      },
      claims: () => app.go("kubernetes", "claims"),
      services: () => app.go("kubernetes", "services"),
      images: () => app.go("images"),
      volumes: () => app.go("volumes"),
      networks: () => app.go("networks"),
      models: () => app.go("models"),
      "models-report": async () => {
        await app.go("models");
        const until = performance.now() + 1500;
        let prompt;
        while (!(prompt = document.getElementById("model-prompt")) && performance.now() < until) {
          await new Promise((resolve) => setTimeout(resolve, 20));
        }
        if (!prompt) return;
        prompt.value = "Name the color of a clear daytime sky.";
        prompt.dispatchEvent(new Event("input"));
        prompt.form.requestSubmit();
      },
      settings: () => app.go("settings"),
      "settings-engine-config": async () => {
        await app.openSettingsSection("engine-settings");
        document.getElementById("edit-engine-configuration")?.click();
      },
      "settings-search": async () => {
        await app.go("settings");
        const input = document.getElementById("settings-search");
        input.value = "Virtual machine";
        input.dispatchEvent(new Event("input"));
        input.focus();
      },
      tutorials: () => app.go("tutorials"),
      "settings-builders": async () => {
        await app.openSettingsSection("builder-settings");
        const until = performance.now() + 1500;
        let row;
        while (!(row = document.querySelector('[data-builder="ci-builder"]')) && performance.now() < until) {
          await new Promise(resolve => setTimeout(resolve, 20));
        }
        row?.click();
      },
      extensions: () => app.go("extensions"),
      "extension-view": () => app.go("extensions"),
      "settings-vm": () => app.openSettingsSection("vm-settings"),
      "settings-notifications": () => app.openSettingsSection("notification-settings"),
      "settings-sync": () => app.openSettingsSection("sync-settings"),
      "settings-network": () => app.openSettingsSection("network-settings"),
      "settings-cli": () => app.openSettingsSection("terminal-command"),
      "settings-updates": app.openUpdates,
      "update-source": app.openUpdates,
      "update-admin": async () => { await app.openUpdates(); await app.runUpgrade(); },
      "docker-settings": () => app.openSettingsSection("engine-settings"),
      analysis: async () => {
        await app.go("images");
        app.openDetail("images", images[0].Id, "analysis");
      },
      "analysis-report": async () => {
        await app.go("images");
        app.openDetail("images", images[0].Id, "analysis");
        document.querySelector(".image-analysis .primary").click();
      },
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
      "practice-tracks": () => app.go("lab-containers"),
      "empty-volumes": () => app.go("volumes"),
      // The run form, filled in, with the options open.
      "run-container": () => {
        app.go("containers");
        const form = app.run().querySelector("form");
        const set = (selector, value) => { const el = form.querySelector(selector); el.value = value; };
        set("#run-image", "nginx:alpine");
        set("#run-name", "web");
        set(".run-host", "8080");
        set(".run-container", "80");
        form.querySelector("details").open = true;
        form.querySelector("#run-volumes ~ .run-add").click();
        set(".run-source", "site");
        set(".run-target", "/usr/share/nginx/html");
        form.querySelector(".run-readonly").checked = true;
        set("#run-restart", "unless-stopped");
        form.querySelector("#run-image").focus();
      },
      // The image list: each image has Run (shown on the last one).
      "run-image": () => {
        app.go("images");
        [...document.querySelectorAll(".list-images button[aria-label=Run]")].pop().focus();
      },
      "run-from-image": () => { app.go("images"); app.run("alpine:3.22"); },
    };
    if (view === "empty" && params.get("tab")) app.go(params.get("tab"));
    await (scenes[view] || (() => {}))();
    document.documentElement.dataset.ready = "1";
  });
})();
