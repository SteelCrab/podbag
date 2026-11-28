//! A backend on sample data, with no engine, in the real API's types. For previews and
//! UI tests.
//!
//! The mock changes its lists, so a UI can show the result:
//! `act`, `remove`, `rename` and `create_volume` change them and send
//! [`Event::Changed`]; a pull streams progress and adds the image (a name with
//! `notfound` fails); a followed log gets a line a second; `engine(true)` starts a
//! stopped VM; `vm_set` keeps the values; image compression, like the engine, starts a
//! stopped VM. `run_container` pulls an image it lacks (three lines of progress; a name
//! with `notfound` fails), then adds a running container. Of the Kubernetes contexts `dev`, `staging` and `ocp`, only
//! `ocp` is OpenShift: it alone has Routes. Migrate lists four more images, from other
//! registries. The local model runner starts off and empty; a pull adds the model and a
//! run answers "Blue.". The VM shares `/Users`; synchronized shares are kept in memory.

use crate::backend::Api;
use crate::builders;
use crate::compression::{Compression, Mode};
use crate::desktop::Connection;
use crate::dockercmd::DockerCommand;
use crate::engine::{EngineStatus, engine_kind};
use crate::events::{Event, Events};
use crate::images::{self, ImportImage, LocationInfo, Search};
use crate::kubernetes::{Resource, Snapshot, route_url};
use crate::lock;
use crate::migration::{self, Listing, Request};
use crate::model_runner::{self, Failure, Generation, Model, Reply};
use crate::run::pull_reference;
use crate::settings::Settings;
use crate::strings;
use crate::terminal_cli;
use crate::tray::{EngineState, TrayInput, TrayModel};
use crate::updates::{Check, Updates, Upgraded};
use crate::vm::{
    SharedPaths, SyncAction, SyncShare, VmDisk, VmLimits, VmSettings, VmStatus, VmValues,
};
use crate::{NetworkPreferences, NetworkSettings, RegistryProxySettings};
use podbag::client::{
    Choice, CompressionSizes, ContainerSummary, Features, ImageSummary, Ipam, IpamConfig, Kind,
    NetworkSummary, Port, Pruned, RunSpec, SearchResult, VolumeSummary,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

#[derive(Default)]
struct PreviewBuilders {
    selected: bool,
    running: bool,
    removed: bool,
}

impl PreviewBuilders {
    fn catalog(&self, context: &str) -> builders::Catalog {
        let mut entries = vec![builders::Builder {
            name: context.into(),
            driver: "docker".into(),
            current: !self.selected,
            managed: false,
        }];
        if !self.removed {
            entries.push(builders::Builder {
                name: "local-builder".into(),
                driver: "docker-container".into(),
                current: self.selected,
                managed: true,
            });
        }
        builders::Catalog {
            availability: builders::Availability::Ready,
            scope: Some(context.into()),
            context: Some(context.into()),
            outside_scope: false,
            builders: entries,
        }
    }
}

/// Which preview view to start from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scenario {
    /// podbag on macOS, the VM running, sample data.
    #[default]
    Default,
    /// No containers and no volumes.
    Empty,
    /// The VM is stopped.
    VmStopped,
    /// The engine does not answer, on Linux.
    Offline,
    /// Docker Desktop is the engine.
    Docker,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Scenario> {
        Some(match s {
            "default" | "" => Scenario::Default,
            "empty" => Scenario::Empty,
            "vm" => Scenario::VmStopped,
            "offline" => Scenario::Offline,
            "docker" => Scenario::Docker,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub scenario: Scenario,
    /// Some(n): the containers are n synthetic rows, for long lists.
    pub rows: Option<usize>,
    /// `features` all on (stats, top, pause, rename); else they are off.
    pub all_features: bool,
    /// The first language setting ("en" when none).
    pub language: Option<String>,
    /// A newer release is known.
    pub update: bool,
    /// The first launch: the star prompt shows once.
    pub first_launch: bool,
    /// No waiting: what takes a while happens at once (tests).
    pub instant: bool,
    /// The question about image compression shows: a new install, or an update from a
    /// version without it.
    pub compression_prompt: bool,
    pub compression: CompressionOptions,
    pub models: ModelOptions,
    pub analysis: ImageAnalysisOptions,
    /// The platform instead of macOS: `linux` (no VM) or `windows` (a WSL VM whose
    /// resources the app does not set).
    pub platform: Option<&'static str>,
    /// The VM has a synchronized share at start.
    pub sync_share: bool,
    /// Where the mock notes the calls a test checks.
    pub calls: Calls,
}

#[derive(Clone, Debug, Default)]
pub struct ImageAnalysisOptions {
    pub failure: Option<crate::image_analysis::Failure>,
    pub empty: bool,
    pub gate: Gate,
}

/// The local model runner's cases.
#[derive(Clone, Debug, Default)]
pub struct ModelOptions {
    /// Ollama is not installed: Start fails.
    pub missing: bool,
    /// The runner runs at start, with a loaded model.
    pub running: bool,
    /// A run waits until this gate opens.
    pub gate: Gate,
}

/// Holds work until it opens; open at first. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub fn close(&self) {
        *lock(&self.0.0) = true;
    }

    pub fn open(&self) {
        *lock(&self.0.0) = false;
        self.0.1.notify_all();
    }

    fn pass(&self) {
        let mut closed = lock(&self.0.0);
        while *closed {
            closed = self.0.1.wait(closed).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// The calls a test checks, in order: `run_container <spec as JSON>`,
/// `kube_list <kind> <context>`, `kube_open_route <context> <namespace> <name>
/// <address>`, `models <action>`, `vm_set <shared paths>` and `vm_sync <action>
/// [name]`. Clones share the list.
#[derive(Clone, Debug, Default)]
pub struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    fn push(&self, call: String) {
        lock(&self.0).push(call);
    }

    /// The calls so far.
    pub fn all(&self) -> Vec<String> {
        lock(&self.0).clone()
    }

    /// The calls whose name is `name`, without the name.
    pub fn of(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name} ");
        self.all()
            .iter()
            .filter_map(|c| c.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }
}

/// The engine's image compression, and the cases the UI must handle.
#[derive(Clone, Debug)]
pub struct CompressionOptions {
    /// On at start.
    pub on: bool,
    /// Seconds an image stays unused before it is compressed.
    pub after: u64,
    /// Replies name no mode.
    pub broken: bool,
    /// An environment setting keeps the mode as it is.
    pub kept: bool,
    /// Changes fail with this error.
    pub fail: Option<String>,
    /// The VM's state cannot be read.
    pub vm_error: bool,
}

impl Default for CompressionOptions {
    fn default() -> Self {
        CompressionOptions {
            on: false,
            after: 7 * 86_400,
            broken: false,
            kept: false,
            fail: None,
            vm_error: false,
        }
    }
}

impl Options {
    /// From the environment: PODBAG_MOCK_SCENARIO (default, empty, vm, offline, docker),
    /// PODBAG_MOCK_ROWS=N, PODBAG_MOCK_FEATURES=all, PODBAG_MOCK_LANG (en, ko, …, system),
    /// PODBAG_MOCK_COMPRESSION (`on`, `ask` or both, comma-separated),
    /// PODBAG_MOCK_MODELS (`running` or `missing`), PODBAG_MOCK_PLATFORM (`linux`,
    /// `windows`), PODBAG_MOCK_SYNC=1 (a synchronized share).
    pub fn from_env() -> Options {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let compression = var("PODBAG_MOCK_COMPRESSION").unwrap_or_default();
        let has = |word: &str| compression.split(',').any(|w| w.trim() == word);
        let models = var("PODBAG_MOCK_MODELS").unwrap_or_default();
        Options {
            models: ModelOptions {
                missing: models == "missing",
                running: models == "running",
                ..ModelOptions::default()
            },
            platform: match var("PODBAG_MOCK_PLATFORM").as_deref() {
                Some("linux") => Some("linux"),
                Some("windows") => Some("windows"),
                _ => None,
            },
            sync_share: var("PODBAG_MOCK_SYNC").is_some_and(|v| v != "0"),
            compression_prompt: has("ask"),
            compression: CompressionOptions {
                on: has("on"),
                ..CompressionOptions::default()
            },
            scenario: var("PODBAG_MOCK_SCENARIO")
                .and_then(|s| Scenario::parse(&s))
                .unwrap_or_default(),
            rows: var("PODBAG_MOCK_ROWS").and_then(|v| v.parse().ok()),
            all_features: var("PODBAG_MOCK_FEATURES").as_deref() == Some("all"),
            language: var("PODBAG_MOCK_LANG"),
            ..Options::default()
        }
    }
}

struct State {
    containers: Vec<ContainerSummary>,
    images: Vec<ImageSummary>,
    volumes: Vec<VolumeSummary>,
    networks: Vec<NetworkSummary>,
    kube: Value,
    engine: EngineStatus,
    vm: VmStatus,
    token: u64,
    selected: String,
    settings: Settings,
    docker: DockerCommand,
    terminal: terminal_cli::Status,
    star_prompt: bool,
    logs_generation: u64,
    logs_following: bool,
    update_found: bool,
    compression: Compression,
    extensions: Vec<crate::extensions::Installed>,
    builders: BTreeMap<String, PreviewBuilders>,
    disk: crate::DiskStorage,
    config_text: Option<String>,
    network_settings: NetworkSettings,
    registry_proxy: RegistryProxySettings,
    /// The local model runner runs, and the models it has.
    model_runner: bool,
    models: Vec<Model>,
    sync: Vec<SyncShare>,
}

/// The mocked backend. Use it through [`crate::Backend::mock`].
pub struct Mock {
    state: Arc<Mutex<State>>,
    training: Mutex<crate::mock_training::Training>,
    image_scan: Mutex<()>,
    /// Launch at login.
    login: AtomicBool,
    events: Events,
    now: i64,
    opts: Options,
}

fn port(private: u16, public: u16) -> Port {
    Port {
        ip: "0.0.0.0".into(),
        private_port: private,
        public_port: public,
        kind: "tcp".into(),
    }
}

/// Days since the epoch to the civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// JavaScript's `toISOString()` of Unix seconds: `2026-11-23T10:00:00.000Z`.
fn iso_time(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

fn sample_containers(now: i64) -> Vec<ContainerSummary> {
    let c = |id: &str,
             name: &str,
             image: &str,
             command: &str,
             created: i64,
             state: &str,
             status: &str,
             ports: Vec<Port>| ContainerSummary {
        id: id.into(),
        names: vec![format!("/{name}")],
        image: image.into(),
        command: command.into(),
        created,
        state: state.into(),
        status: status.into(),
        ports,
    };
    vec![
        c(
            "a1b2c3d4e5f60718",
            "web",
            "nginx:alpine",
            "nginx -g 'daemon off;'",
            now - 7200,
            "running",
            "Up 2 hours",
            vec![port(80, 8080)],
        ),
        c(
            "b2c3d4e5f6a70819",
            "db",
            "postgres:16-alpine",
            "docker-entrypoint.sh postgres",
            now - 86400 * 3,
            "running",
            "Up 3 days",
            vec![port(5432, 5432)],
        ),
        c(
            "c3d4e5f6a7b8091a",
            "cache",
            "redis:7-alpine",
            "redis-server",
            now - 86400 * 3,
            "running",
            "Up 3 days",
            vec![port(6379, 0)],
        ),
        c(
            "d4e5f6a7b8c9102b",
            "worker",
            "registry.example.com/team/worker:1.4.2",
            "/app/worker --queue jobs",
            now - 3600 * 5,
            "exited",
            "Exited (1) 4 hours ago",
            vec![],
        ),
        c(
            "e5f6a7b8c9d0213c",
            "migrate",
            "myapp:1.4",
            "./migrate up",
            now - 86400 * 9,
            "exited",
            "Exited (0) 9 days ago",
            vec![],
        ),
    ]
}

/// `n` synthetic containers, as in the UI prototypes' long-list tests.
fn rows(n: usize, now: i64) -> Vec<ContainerSummary> {
    const PREFIXES: [&str; 5] = ["web", "api", "db", "cache", "worker"];
    const IMAGES: [&str; 5] = [
        "docker.io/library/nginx:1.27",
        "ghcr.io/example/api:2.3.1",
        "docker.io/library/postgres:16",
        "docker.io/library/redis:7.4",
        "quay.io/example/worker:0.9",
    ];
    const INNER: [u16; 5] = [80, 8080, 5432, 6379, 9000];
    (0..n)
        .map(|i| {
            let p = i % 5;
            let running = i % 3 != 0;
            let k = i as i64;
            let (age, ago) = match i % 4 {
                0 => ((1 + k % 59) * 60, format!("{} minutes", 1 + k % 59)),
                1 => ((1 + k % 23) * 3600, format!("{} hours", 1 + k % 23)),
                2 => ((1 + k % 6) * 86400, format!("{} days", 1 + k % 6)),
                _ => ((1 + k % 4) * 604_800, format!("{} weeks", 1 + k % 4)),
            };
            ContainerSummary {
                id: format!(
                    "{:016x}",
                    (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1 << 60
                ),
                names: vec![format!("/{}-{i:05}", PREFIXES[p])],
                image: IMAGES[p].into(),
                command: String::new(),
                created: now - age,
                state: if running { "running" } else { "exited" }.into(),
                status: if running {
                    format!("Up {ago}")
                } else {
                    format!("Exited (0) {ago} ago")
                },
                ports: if running {
                    vec![Port {
                        ip: "127.0.0.1".into(),
                        private_port: INNER[p],
                        public_port: (8000 + i % 1000) as u16,
                        kind: "tcp".into(),
                    }]
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

fn sample_images(now: i64) -> Vec<ImageSummary> {
    let i = |id: &str, tag: &str, days: i64, size: i64, containers: i64| ImageSummary {
        id: id.into(),
        repo_tags: vec![tag.into()],
        created: now - 86400 * days,
        size,
        containers,
        podbag_compression: None,
    };
    // Unused for a long time, so the engine compressed it: 3.9 MB on disk, 54% smaller.
    let alpine = ImageSummary {
        podbag_compression: Some(CompressionSizes {
            size: 3_900_000,
            original_size: 8_520_000,
        }),
        ..i("sha256:5f5b2e0a0c6d88ee", "alpine:3.22", 60, 8_520_000, 0)
    };
    vec![
        i("sha256:9d1f6a0c3e2b44aa", "nginx:alpine", 12, 52_400_000, 1),
        i(
            "sha256:8c2e5b1d7f3a55bb",
            "postgres:16-alpine",
            20,
            251_000_000,
            1,
        ),
        i(
            "sha256:7b3d4c2e8a4b66cc",
            "redis:7-alpine",
            30,
            41_200_000,
            1,
        ),
        i(
            "sha256:6a4c3d1f9b5c77dd",
            "registry.example.com/team/worker:1.4.2",
            2,
            183_000_000,
            1,
        ),
        alpine,
    ]
}

fn sample_volumes(now: i64) -> Vec<VolumeSummary> {
    let v = |name: &str, days: i64| VolumeSummary {
        name: name.into(),
        driver: "local".into(),
        mountpoint: format!("/var/lib/podbag/volumes/{name}/_data"),
        created_at: iso_time(now - 86400 * days),
    };
    vec![
        v("pgdata", 3),
        v("redis-data", 3),
        v("3f2a9c41d7e8b6a5c0f1e2d3b4a59687", 40),
    ]
}

fn sample_networks() -> Vec<NetworkSummary> {
    let n = |name: &str, id: &str, driver: &str, ipam: &[(&str, &str)]| NetworkSummary {
        name: name.into(),
        id: id.into(),
        driver: driver.into(),
        scope: "local".into(),
        created: String::new(),
        ipam: Ipam {
            config: ipam
                .iter()
                .map(|(subnet, gateway)| IpamConfig {
                    subnet: subnet.to_string(),
                    gateway: gateway.to_string(),
                })
                .collect(),
        },
    };
    vec![
        n(
            "bridge",
            "4e1c0b8a2f6d9e7c",
            "bridge",
            &[("10.77.0.0/16", "10.77.0.1")],
        ),
        n(
            "app",
            "5f2d1c9b3a7e0f8d",
            "bridge",
            &[("10.77.1.0/24", "10.77.1.1")],
        ),
        n("host", "6a3e2d0c4b8f1a9e", "host", &[]),
        n("none", "7b4f3e1d5c9a2b0f", "null", &[]),
    ]
}

fn sample_kube() -> Value {
    json!({
        "pods": [
            { "metadata": { "name": "web", "namespace": "default" }, "spec": { "containers": [{ "name": "nginx" }], "nodeName": "worker-1" }, "status": { "phase": "Running", "containerStatuses": [{ "name": "nginx", "ready": true }] } },
            { "metadata": { "name": "web", "namespace": "staging" }, "spec": { "containers": [{ "name": "nginx" }] }, "status": { "phase": "Pending" } }
        ],
        "deployments": [
            { "metadata": { "name": "frontend", "namespace": "default" }, "spec": { "replicas": 3, "template": { "spec": { "containers": [{ "image": "nginx:alpine" }] } } }, "status": { "readyReplicas": 3 } }
        ],
        "claims": [
            { "metadata": { "name": "postgres-data", "namespace": "default" }, "spec": { "storageClassName": "standard" }, "status": { "phase": "Bound", "capacity": { "storage": "20Gi" } } }
        ],
        "services": [
            { "metadata": { "name": "frontend", "namespace": "default" }, "spec": { "type": "NodePort", "clusterIP": "10.43.0.80", "ports": [{ "port": 80, "nodePort": 30080, "protocol": "TCP" }] } }
        ],
        "routes": [
            { "kind": "Route", "metadata": { "name": "shop", "namespace": "default" }, "spec": { "host": "shop.apps.ocp.example.com", "path": "/cart", "to": { "kind": "Service", "name": "frontend" }, "tls": { "termination": "edge" } } },
            { "kind": "Route", "metadata": { "name": "api", "namespace": "default" }, "spec": { "host": "api.apps.ocp.example.com", "to": { "kind": "Service", "name": "api" } } },
            { "kind": "Route", "metadata": { "name": "docs", "namespace": "staging" }, "spec": { "host": "docs-staging.apps.ocp.example.com", "to": { "kind": "Service", "name": "docs" }, "tls": { "termination": "passthrough" } } }
        ]
    })
}

const LIMITS: VmLimits = VmLimits {
    cpus_min: 1,
    cpus_max: 10,
    memory_min_mib: 1024,
    memory_max_mib: 20480,
    mac_memory_mib: 24576,
};

fn values(cpus: Option<u32>, memory_mib: Option<u64>, idle: Option<u64>) -> VmValues {
    VmValues {
        cpus,
        memory_mib,
        idle_seconds: idle,
    }
}

fn running_vm() -> VmStatus {
    VmStatus {
        platform: "macos".into(),
        state: "running".into(),
        resources_editable: true,
        in_use: Some(values(Some(4), Some(4096), Some(300))),
        next: Some(values(Some(4), Some(6144), Some(300))),
        saved: Some(values(None, Some(6144), None)),
        overridden_by: Vec::new(),
        pending: true,
        limits: Some(LIMITS),
        uptime_seconds: Some(7560),
        containers: Some(3),
        cpu_percent: Some(12.5),
        memory_used_mib: Some(1350),
        memory_total_mib: Some(3900),
        disk: Some(VmDisk {
            path: "/Users/me/.podbag/vm/disk.img".into(),
            allocated_bytes: 5_800_000_000,
            size_bytes: 68_719_476_736,
        }),
        shared_paths: Some(SharedPaths {
            in_use: Some(vec!["/Users".into()]),
            next: vec!["/Users".into()],
        }),
    }
}

/// A WSL VM: Windows reports only its state.
fn windows_vm() -> VmStatus {
    VmStatus {
        platform: "windows".into(),
        state: "running".into(),
        resources_editable: false,
        ..VmStatus::default()
    }
}

/// The model the runner's samples have.
const DEMO_MODEL: (&str, u64) = ("smollm2:135m", 92_078_644);

fn sample_share(name: &str, source: &str) -> SyncShare {
    SyncShare {
        name: name.into(),
        source: source.into(),
        volume: format!("podbag-sync-{name}"),
        conflicts: Vec::new(),
        last_error: None,
    }
}

/// Images that only Migrate lists: tags of several registries, and one without a tag.
fn migration_images(now: i64) -> Value {
    let image = |id: &str, tags: &[&str]| {
        json!({"Id": id, "RepoTags": tags, "Created": now - 86_400 * 5, "Size": 48_000_000,
               "Containers": 0})
    };
    json!([
        image(
            "sha256:103afa055a409b34",
            &["example/api:1.5", "registry.gitlab.com/example/api:1.5"]
        ),
        image("sha256:158bb8104d657e3f", &["ghcr.io/example/frontend:2.0"]),
        image("sha256:498bdb036ad35429", &["quay.io/example/proxy:0.8"]),
        image("sha256:8ab61e38dd2ac135", &["<none>:<none>"]),
    ])
}

fn stopped_vm() -> VmStatus {
    VmStatus {
        state: "stopped".into(),
        in_use: None,
        next: Some(values(Some(4), Some(4096), Some(300))),
        saved: Some(values(Some(4), Some(4096), None)),
        pending: false,
        uptime_seconds: None,
        containers: None,
        cpu_percent: None,
        memory_used_mib: None,
        memory_total_mib: None,
        disk: Some(VmDisk {
            path: "/Users/me/.podbag/vm/disk.img".into(),
            allocated_bytes: 3_400_000_000,
            size_bytes: 68_719_476_736,
        }),
        shared_paths: Some(SharedPaths {
            in_use: None,
            next: vec!["/Users".into()],
        }),
        ..running_vm()
    }
}

/// The engine's state for a scenario, a platform other than macOS and the engine chosen.
fn engine_info(scenario: Scenario, platform: Option<&'static str>, selected: &str) -> EngineStatus {
    let docker = scenario == Scenario::Docker;
    let mut e = EngineStatus {
        platform: "macos",
        engine: if docker { "docker" } else { "podbag" },
        engine_name: if docker { "Docker" } else { "podbag" }.into(),
        endpoint: "unix:///Users/me/.podbag/run/docker.sock".into(),
        shell: true,
        reachable: !matches!(scenario, Scenario::Offline | Scenario::VmStopped),
        vm: Some(if scenario == Scenario::VmStopped { "stopped" } else { "running" }.into()),
        version: Some("podbag 0.1.0 (API 1.47)".into()),
        error: match scenario {
            Scenario::VmStopped => Some("vm-stopped".into()),
            Scenario::Offline => Some(
                "cannot connect to unix:///Users/me/.podbag/run/docker.sock: No such file or directory"
                    .into(),
            ),
            _ => None,
        },
    };
    if docker {
        e.endpoint = "unix:///Users/me/.docker/run/docker.sock".into();
        e.vm = None;
        e.version = Some("Docker Desktop 4.50.0 29.0.1 (API 1.52)".into());
    }
    if scenario == Scenario::Offline || platform == Some("linux") {
        e.platform = "linux";
        e.vm = None;
        e.endpoint = "unix:///run/user/1000/podbag/docker.sock".into();
    }
    if platform == Some("windows") {
        e.platform = "windows";
        e.endpoint = "npipe:////./pipe/podbag".into();
    }
    if let Some(choice) = Choice::parse(selected)
        && engine_kind(&choice) != e.engine
    {
        e.engine = engine_kind(&choice);
        e.engine_name = choice.label();
        e.endpoint = if choice == Choice::Kiri {
            "unix:///Users/me/.kiri/run/docker.sock".into()
        } else {
            selected.into()
        };
        e.vm = None;
        e.version = Some(selected.into());
    }
    e
}

fn log_lines() -> Vec<String> {
    let mut v: Vec<String> = [
        "/docker-entrypoint.sh: Configuration complete; ready for start up",
        "2026/11/20 03:12:01 [notice] 1#1: using the \"epoll\" event method",
        "2026/11/20 03:12:01 [notice] 1#1: nginx/1.27.3",
        "2026/11/20 03:12:01 [notice] 1#1: start worker processes",
    ]
    .map(String::from)
    .into();
    v.extend((0..24).map(access_line));
    v
}

fn access_line(i: usize) -> String {
    format!(
        "10.77.0.1 - - [20/Nov/2026:03:{:02}:07 +0000] \"GET /api/items?page={} HTTP/1.1\" 200 {} \"-\" \"curl/8.11.0\"",
        (14 + i) % 60,
        i + 1,
        1200 + i * 37
    )
}

fn hub() -> Vec<SearchResult> {
    let r = |name: &str, description: &str, star_count: u64, is_official: bool| SearchResult {
        name: name.into(),
        description: description.into(),
        star_count,
        is_official,
    };
    vec![
        r("nginx", "Official build of Nginx.", 21034, true),
        r(
            "nginxinc/nginx-unprivileged",
            "Unprivileged NGINX Dockerfiles",
            172,
            false,
        ),
        r(
            "nginx/nginx-ingress",
            "NGINX and NGINX Plus Ingress Controllers for Kubernetes",
            109,
            false,
        ),
        r(
            "bitnami/nginx",
            "Bitnami container image for NGINX",
            197,
            false,
        ),
    ]
}

fn docker_cmd(is_podbag: bool) -> DockerCommand {
    DockerCommand {
        installed: true,
        name: if is_podbag {
            "podbag"
        } else {
            "Docker Desktop"
        }
        .into(),
        is_podbag,
        from_env: false,
        context: Some(if is_podbag { "podbag" } else { "desktop-linux" }.into()),
        previous: is_podbag.then(|| "desktop-linux".into()),
    }
}

const TERMINAL_LINK: &str = "/usr/local/bin/podbag";

impl Mock {
    pub fn new(opts: Options, events: Events) -> Mock {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let scenario = opts.scenario;
        let mut containers = match opts.rows {
            Some(n) => rows(n, now),
            None => sample_containers(now),
        };
        let mut volumes = sample_volumes(now);
        if scenario == Scenario::Empty {
            containers.clear();
            volumes.clear();
        }
        let engine = engine_info(scenario, opts.platform, "");
        let state = State {
            containers,
            images: sample_images(now),
            volumes,
            networks: sample_networks(),
            kube: sample_kube(),
            selected: engine.engine.to_string(),
            engine,
            vm: if opts.platform == Some("windows") {
                windows_vm()
            } else if scenario == Scenario::VmStopped {
                stopped_vm()
            } else {
                running_vm()
            },
            token: 1,
            settings: Settings {
                language: opts.language.clone().unwrap_or_else(|| "en".into()),
                compression_prompt_seen: !opts.compression_prompt,
                ..Settings::default()
            }
            .checked(),
            docker: docker_cmd(true),
            terminal: terminal_cli::Status {
                state: "other",
                path: Some(TERMINAL_LINK.into()),
                target: TERMINAL_LINK.into(),
                problem: None,
            },
            star_prompt: opts.first_launch,
            logs_generation: 0,
            logs_following: false,
            update_found: false,
            compression: Compression {
                mode: Some(if opts.compression.on {
                    Mode::Idle
                } else {
                    Mode::Off
                }),
                after: Some(opts.compression.after),
            },
            extensions: Vec::new(),
            builders: BTreeMap::new(),
            disk: crate::DiskStorage {
                path: "/Users/me/.podbag/vm/disk.img".into(),
                exists: true,
                configured: false,
                editable: false,
                size_bytes: 32 << 30,
                allocated_bytes: 5 << 30,
                filesystem_bytes: Some(32 << 30),
                growth_pending: false,
                max_size_gib: 1024,
                backup_path: None,
            },
            config_text: Some("# Local engine settings\nstore_limit = \"4 GiB\"\n".into()),
            network_settings: NetworkSettings {
                preferences: NetworkPreferences {
                    default_subnet: "10.77.0.0/16".into(),
                    default_ipv6_subnet: None,
                    embedded_dns: false,
                    dns_filter: crate::DnsFilter::None,
                },
                revision: "0".into(),
                active_subnet: Some("10.77.0.0/16".into()),
                active_ipv6_subnet: None,
                overrides: Vec::new(),
            },
            registry_proxy: RegistryProxySettings::default(),
            model_runner: opts.models.running,
            models: if opts.models.running {
                vec![Model {
                    name: DEMO_MODEL.0.into(),
                    size: DEMO_MODEL.1,
                    loaded: true,
                }]
            } else {
                Vec::new()
            },
            sync: if opts.sync_share {
                vec![sample_share("project", "/Users/me/projects/app")]
            } else {
                Vec::new()
            },
        };
        Mock {
            state: Arc::new(Mutex::new(state)),
            training: Mutex::new(crate::mock_training::Training::new(
                scenario == Scenario::Offline,
            )),
            image_scan: Mutex::new(()),
            login: AtomicBool::new(false),
            events,
            now,
            opts,
        }
    }

    /// The time the sample data is relative to (Unix seconds).
    pub fn now(&self) -> i64 {
        self.now
    }

    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn token(&self) -> u64 {
        self.st().token
    }

    fn changed(&self, kind: Kind) {
        let session = self.token();
        self.events.send(Event::Changed {
            session,
            kind: kind.event_type().into(),
        });
    }

    /// Wait `ms` milliseconds (not with `instant`).
    fn wait(&self, ms: u64) {
        if !self.opts.instant {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }

    /// Run `f` after `ms` milliseconds on a thread (at once with `instant`).
    fn later(&self, ms: u64, f: impl FnOnce(&Events, &Mutex<State>) + Send + 'static) {
        let (events, state) = (self.events.clone(), self.state.clone());
        if self.opts.instant {
            f(&events, &state);
            return;
        }
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            f(&events, &state);
        });
    }

    fn set_vm_running(s: &mut State, on: bool) {
        s.engine.vm = Some(if on { "running" } else { "stopped" }.into());
        s.engine.reachable = on;
        s.engine.error = (!on).then(|| "vm-stopped".into());
        if s.vm.platform == "windows" {
            s.vm.state = if on { "running" } else { "stopped" }.into();
            return;
        }
        let (next, saved, overridden) = (s.vm.next, s.vm.saved, s.vm.overridden_by.clone());
        let shares = s.vm.shared_paths.clone().unwrap_or_default().next;
        s.vm = if on {
            VmStatus {
                in_use: next,
                next,
                pending: false,
                uptime_seconds: Some(0),
                ..running_vm()
            }
        } else {
            VmStatus {
                next,
                ..stopped_vm()
            }
        };
        s.vm.saved = saved;
        s.vm.overridden_by = overridden;
        s.vm.shared_paths = Some(SharedPaths {
            in_use: on.then(|| shares.clone()),
            next: shares,
        });
    }

    /// The objects of `kind` in `context` (None or empty: `dev`). Only `ocp` has Routes;
    /// other clusters refuse the type, as kubectl does.
    fn kube_items(
        &self,
        kind: Resource,
        all: bool,
        context: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        if matches!(kind, Resource::Routes) && !mock_openshift(context) {
            return Err(
                "kubectl (exit status: 1): error: the server doesn't have a resource type \"routes\""
                    .into(),
            );
        }
        Ok(self.st().kube[kind.key()]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|i| all || i["metadata"]["namespace"] == "default")
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Only the `ocp` context is OpenShift.
fn mock_openshift(context: Option<&str>) -> bool {
    context == Some("ocp")
}

fn no_such(what: &str, id: &str) -> String {
    format!("No such {what}: {id}")
}

impl Api for Mock {
    fn events(&self) -> &Events {
        &self.events
    }

    fn connection(&self) -> Connection {
        let s = self.st();
        Connection {
            token: s.token,
            name: s.selected.clone(),
            local: s.selected == "podbag",
        }
    }

    fn contexts(&self) -> Vec<String> {
        vec!["desktop-linux".into(), "orbstack".into()]
    }

    fn engine(&self, start: bool) -> EngineStatus {
        let status = {
            let mut s = self.st();
            if start && s.engine.vm.as_deref() == Some("stopped") {
                Mock::set_vm_running(&mut s, true);
            }
            s.engine.clone()
        };
        self.events.send(Event::StatusChanged);
        status
    }

    fn set_engine(&self, name: &str) -> Result<(), String> {
        Choice::parse(name).ok_or_else(|| format!("unknown engine {name}"))?;
        {
            let mut s = self.st();
            s.selected = name.to_string();
            s.token += 1;
            s.engine = engine_info(self.opts.scenario, self.opts.platform, name);
        }
        self.events.send(Event::StatusChanged);
        Ok(())
    }

    fn containers(&self) -> Result<Vec<ContainerSummary>, String> {
        Ok(self.st().containers.clone())
    }

    fn images(&self) -> Result<Vec<ImageSummary>, String> {
        Ok(self.st().images.clone())
    }

    fn volumes(&self) -> Result<Vec<VolumeSummary>, String> {
        Ok(self.st().volumes.clone())
    }

    fn networks(&self) -> Result<Vec<NetworkSummary>, String> {
        Ok(self.st().networks.clone())
    }

    /// A sample, whatever the object.
    fn inspect(&self, kind: Kind, id: &str) -> Result<Value, String> {
        Ok(json!({
            "Id": id, "Name": "/web", "Created": "2026-11-20T01:12:00Z",
            "State": { "Status": "running", "Running": true, "Pid": 4182, "StartedAt": "2026-11-20T01:12:01Z" },
            "Config": { "Image": "nginx:alpine", "Cmd": ["nginx", "-g", "daemon off;"], "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "NGINX_VERSION=1.27.3"], "Tty": false },
            "HostConfig": { "PortBindings": { "80/tcp": [{ "HostIp": "", "HostPort": "8080" }] }, "RestartPolicy": { "Name": "no" } },
            "NetworkSettings": { "IPAddress": "10.77.0.2" },
            "Kind": kind.segment(),
        }))
    }

    fn analyze_image(
        &self,
        session: u64,
        id: &str,
    ) -> Result<crate::image_analysis::Report, crate::image_analysis::Failure> {
        use crate::image_analysis::{Failure, Finding, Package, Report, Severity};
        if self.token() != session {
            return Err(Failure::EngineChanged);
        }
        let _scan = self.image_scan.try_lock().map_err(|_| Failure::Busy)?;
        self.opts.calls.push(format!("analyze_image {id}"));
        self.opts.analysis.gate.pass();
        if self.token() != session {
            return Err(Failure::EngineChanged);
        }
        if !self.st().images.iter().any(|image| image.id == id) {
            return Err(Failure::ImageUnavailable);
        }
        if let Some(error) = self.opts.analysis.failure {
            return Err(error);
        }
        let mut report = Report {
            image_id: id.into(),
            packages: Vec::new(),
            findings: Vec::new(),
        };
        if !self.opts.analysis.empty {
            report.packages.push(Package {
                name: "musl".into(),
                version: "1.2.5-r10".into(),
                target: "alpine".into(),
            });
            report.findings.push(Finding {
                severity: Severity::High,
                advisory: "DEMO-2026-0001".into(),
                package: "musl".into(),
                installed: "1.2.5-r10".into(),
                fixed: "1.2.5-r11".into(),
                target: "alpine".into(),
            });
        }
        Ok(report)
    }

    fn features(&self, _sample: Option<&str>) -> Features {
        let on = self.opts.all_features;
        Features {
            stats: on,
            top: on,
            pause: on,
            rename: on,
        }
    }

    fn container_json(&self, _id: &str, what: &str) -> Result<Value, String> {
        if !self.opts.all_features {
            return Err("not supported".into());
        }
        match what {
            "top" => Ok(json!({
                "Titles": ["PID", "USER", "TIME", "COMMAND"],
                "Processes": [["4182", "root", "0:00", "nginx: master process nginx -g daemon off;"],
                              ["4210", "nginx", "0:03", "nginx: worker process"]]
            })),
            "stats" => Ok(json!({
                "cpu_stats": { "cpu_usage": { "total_usage": 2_000_000 }, "system_cpu_usage": 500_000_000, "online_cpus": 4 },
                "precpu_stats": { "cpu_usage": { "total_usage": 1_000_000 }, "system_cpu_usage": 400_000_000 },
                "memory_stats": { "usage": 9_437_184, "limit": 4_294_967_296u64 },
                "pids_stats": { "current": 2 }
            })),
            _ => Err(format!("unknown request {what}")),
        }
    }

    fn act(&self, id: &str, action: &str) -> Result<(), String> {
        {
            let mut s = self.st();
            let (state, status) = match action {
                "start" | "restart" | "unpause" => ("running", "Up Less than a second"),
                "stop" => ("exited", "Exited (0) Less than a second ago"),
                "pause" => ("paused", "Up Less than a second (Paused)"),
                _ => return Err(format!("unknown action {action}")),
            };
            let c = s
                .containers
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| no_such("container", id))?;
            c.state = state.into();
            c.status = status.into();
        }
        self.changed(Kind::Containers);
        Ok(())
    }

    fn remove(&self, kind: Kind, id: &str, force: bool) -> Result<(), String> {
        {
            let mut s = self.st();
            match kind {
                Kind::Containers => {
                    let i = s
                        .containers
                        .iter()
                        .position(|c| c.id == id)
                        .ok_or_else(|| no_such("container", id))?;
                    let c = &s.containers[i];
                    if matches!(c.state.as_str(), "running" | "paused" | "restarting") && !force {
                        return Err(format!(
                            "cannot remove container {}: container is running: stop the container before removing or force remove",
                            c.name()
                        ));
                    }
                    s.containers.remove(i);
                }
                Kind::Images => {
                    let i = s
                        .images
                        .iter()
                        .position(|o| o.id == id)
                        .ok_or_else(|| no_such("image", id))?;
                    s.images.remove(i);
                }
                Kind::Volumes => {
                    let i = s
                        .volumes
                        .iter()
                        .position(|o| o.name == id)
                        .ok_or_else(|| no_such("volume", id))?;
                    s.volumes.remove(i);
                }
                Kind::Networks => {
                    let i = s
                        .networks
                        .iter()
                        .position(|o| o.id == id)
                        .ok_or_else(|| no_such("network", id))?;
                    if s.networks[i].builtin() {
                        return Err(format!(
                            "{} is a pre-defined network and cannot be removed",
                            s.networks[i].name
                        ));
                    }
                    s.networks.remove(i);
                }
            }
        }
        self.changed(kind);
        Ok(())
    }

    /// Always 2 objects and 61 MB; the lists stay.
    fn prune(&self, _kind: Kind) -> Result<Pruned, String> {
        Ok(Pruned {
            count: 2,
            bytes: 61_000_000,
        })
    }

    fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        {
            let mut s = self.st();
            let new = format!("/{}", name.trim_start_matches('/'));
            if s.containers
                .iter()
                .any(|c| c.id != id && c.names.first() == Some(&new))
            {
                return Err(format!(
                    "Conflict. The container name \"{new}\" is already in use"
                ));
            }
            let c = s
                .containers
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| no_such("container", id))?;
            c.names = vec![new];
        }
        self.changed(Kind::Containers);
        Ok(())
    }

    fn create_volume(&self, name: &str) -> Result<(), String> {
        {
            let mut s = self.st();
            if s.volumes.iter().any(|v| v.name == name) {
                return Err(format!("volume {name} already exists"));
            }
            // Only the name and the driver.
            s.volumes.push(VolumeSummary {
                name: name.into(),
                driver: "local".into(),
                ..VolumeSummary::default()
            });
        }
        self.changed(Kind::Volumes);
        Ok(())
    }

    /// Like the engine: a missing image is pulled first (three lines, 60 ms apart), then
    /// the container runs.
    fn run_container(&self, spec: &RunSpec) -> Result<String, String> {
        self.opts.calls.push(format!(
            "run_container {}",
            serde_json::to_string(spec).unwrap_or_default()
        ));
        spec.check()?;
        let image = spec.image.trim().to_string();
        let name = spec.name.trim();
        let tag = pull_reference(&image);
        let have = |s: &State| {
            s.images
                .iter()
                .any(|i| i.repo_tags.contains(&image) || i.repo_tags.contains(&tag))
        };
        if !have(&self.st()) {
            let session = self.token();
            for (i, line) in [
                format!("Pulling from library/{image}"),
                "Download complete".into(),
                "Pull complete".into(),
            ]
            .into_iter()
            .enumerate()
            {
                if i == 1 && image.contains("notfound") {
                    return Err(format!("{tag}: manifest unknown"));
                }
                self.events.send(Event::PullProgress { session, line });
                self.wait(60);
            }
            let mut s = self.st();
            let n = s.images.len();
            s.images.push(ImageSummary {
                id: format!("sha256:4e3d2c1b{n:08x}"),
                repo_tags: vec![tag.clone()],
                created: self.now,
                size: 9_000_000,
                containers: 0,
                podbag_compression: None,
            });
            drop(s);
            self.changed(Kind::Images);
        }
        let id = {
            let mut s = self.st();
            let id = format!("f00dfeed{:08}", s.containers.len());
            if let Some(i) = s
                .images
                .iter_mut()
                .find(|i| i.repo_tags.contains(&image) || i.repo_tags.contains(&tag))
            {
                i.containers += 1;
            }
            let ports = spec
                .ports
                .iter()
                .map(|p| Port {
                    ip: "0.0.0.0".into(),
                    private_port: p.container,
                    public_port: p.host,
                    kind: if p.protocol.is_empty() {
                        "tcp".into()
                    } else {
                        p.protocol.clone()
                    },
                })
                .collect();
            let created = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(self.now, |d| d.as_secs() as i64);
            s.containers.insert(
                0,
                ContainerSummary {
                    id: id.clone(),
                    names: vec![format!(
                        "/{}",
                        if name.is_empty() { "eager_tesla" } else { name }
                    )],
                    image,
                    command: spec.command.join(" "),
                    created,
                    state: "running".into(),
                    status: "Up 1 second".into(),
                    ports,
                },
            );
            id
        };
        self.changed(Kind::Containers);
        Ok(id)
    }

    fn open_shell(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_terminal(&self, _what: &str) -> Result<(), String> {
        Ok(())
    }

    /// The sample lines after 30 ms, then a line a second until `logs_stop`.
    fn logs_start(&self, _id: &str) -> Result<u64, String> {
        let (session, generation) = {
            let mut s = self.st();
            s.logs_generation += 1;
            s.logs_following = true;
            (s.token, s.logs_generation)
        };
        let instant = self.opts.instant;
        self.later(30, move |events, state| {
            events.send(Event::Logs {
                session,
                generation,
                lines: log_lines(),
            });
            if instant {
                return;
            }
            for i in 24.. {
                std::thread::sleep(Duration::from_secs(1));
                let following = {
                    let s = lock(state);
                    s.logs_following && s.logs_generation == generation
                };
                if !following
                    || !events.send(Event::Logs {
                        session,
                        generation,
                        lines: vec![access_line(i)],
                    })
                {
                    break;
                }
            }
        });
        Ok(generation)
    }

    fn logs_stop(&self) {
        self.st().logs_following = false;
    }

    /// `Live` after 10 ms.
    fn watch(&self, on: bool) {
        let session = self.token();
        self.later(10, move |events, _| {
            events.send(Event::Live { session, on });
        });
    }

    /// Six lines of progress, about 220 ms apart; the image is added.
    fn pull(
        &self,
        image: &str,
        location: Option<&str>,
        host: Option<&str>,
    ) -> Result<String, String> {
        let location = match location {
            Some(id) => Some(images::location(id, host.unwrap_or(""))?),
            None => None,
        };
        let reference = images::reference(image, location.as_ref())?;
        let session = self.token();
        let lines = [
            format!("Trying to pull {reference}..."),
            "Getting image source signatures".into(),
            "Copying blob sha256:4abcf2066143 done".into(),
            "Copying blob sha256:9b3977197b4f done".into(),
            "Copying config sha256:5e8f1c2d3a4b done".into(),
            "Writing manifest to image destination".into(),
        ];
        self.wait(150);
        for (i, line) in lines.into_iter().enumerate() {
            if i > 0 {
                self.wait(220);
            }
            if i == 1 && reference.contains("notfound") {
                return Err(format!("{reference}: manifest unknown"));
            }
            self.events.send(Event::PullProgress { session, line });
        }
        {
            let mut s = self.st();
            let tag = if reference.contains(':') {
                reference.clone()
            } else {
                format!("{reference}:latest")
            };
            if !s.images.iter().any(|i| i.repo_tags.contains(&tag)) {
                let n = s.images.len() as u64;
                s.images.push(ImageSummary {
                    id: format!("sha256:{:016x}", 0x4c0f_fee0_0000_0000u64 + n),
                    repo_tags: vec![tag],
                    created: self.now - 86400,
                    size: 31_400_000,
                    containers: 0,
                    podbag_compression: None,
                });
            }
        }
        self.changed(Kind::Images);
        Ok(reference)
    }

    fn search(&self, term: &str) -> Search {
        let term = term.trim();
        Search {
            supported: true,
            error: None,
            results: if term.is_empty() {
                Vec::new()
            } else {
                hub()
                    .into_iter()
                    .filter(|r| r.name.contains(term))
                    .collect()
            },
        }
    }

    /// The real locations; someone is logged in at GitHub.
    fn registries(&self, hosts: &[(String, String)]) -> Vec<LocationInfo> {
        images::locations(&[("ghcr.io".into(), "octocat".into())], hosts)
    }

    /// The user `denied` is refused, as a registry refuses a wrong password.
    fn registry_login(
        &self,
        location: &str,
        host: &str,
        user: &str,
        _password: &str,
    ) -> Result<(), String> {
        let loc = images::location(location, host)?;
        if loc.needs_host() && loc.host().is_empty() {
            return Err("the registry needs a host".into());
        }
        self.wait(400);
        if user == "denied" {
            return Err("unauthorized: incorrect username or password".into());
        }
        Ok(())
    }

    fn docker_images(&self, _hosts: &[(String, String)]) -> Result<Vec<ImportImage>, String> {
        let d = |name: &str, size: i64, registry: &'static str| ImportImage {
            name: name.into(),
            size,
            registry,
        };
        Ok(vec![
            d("nginx:alpine", 52_400_000, "dockerhub"),
            d("mysql:8.4", 603_000_000, "dockerhub"),
            d("ghcr.io/example/worker:1.4", 183_000_000, "github"),
            d(
                "demo-registry.cn-shanghai.cr.aliyuncs.com/team/api:2",
                76_000_000,
                "alibaba",
            ),
            d("contoso.azurecr.io/team/api:2", 84_000_000, "acr"),
            d(
                "registry.example.com/team/worker:1.4.2",
                183_000_000,
                "custom",
            ),
        ])
    }

    fn import_images(&self, names: &[String]) -> Result<(), String> {
        if names.is_empty() {
            return Err("no images to import".into());
        }
        Ok(())
    }

    fn docker_command(&self) -> DockerCommand {
        self.st().docker.clone()
    }

    /// `DockerSwitching` at once, `DockerChanged` after 650 ms.
    fn docker_switch(&self, to: &str) -> Result<DockerCommand, String> {
        let is_podbag = match to {
            "use" => true,
            "back" => false,
            _ => return Err(format!("unknown switch {to}")),
        };
        self.events.send(Event::DockerSwitching(to.to_string()));
        self.wait(650);
        let now = docker_cmd(is_podbag);
        self.st().docker = now.clone();
        self.events.send(Event::StatusChanged);
        self.events.send(Event::DockerChanged(now.clone()));
        Ok(now)
    }

    fn vm(&self, action: &str) -> Result<(), String> {
        if !matches!(action, "start" | "stop" | "restart") {
            return Err(format!("unknown action {action}"));
        }
        let session = {
            let mut s = self.st();
            if s.selected != "podbag" {
                return Err("VM controls require Podbag".into());
            }
            Mock::set_vm_running(&mut s, action != "stop");
            s.token
        };
        self.events.send(Event::EngineChanged {
            session,
            action: action.to_string(),
        });
        Ok(())
    }

    fn vm_status(&self) -> Result<Option<VmStatus>, String> {
        if self.opts.compression.vm_error {
            return Err("cannot read the VM's state".into());
        }
        let s = self.st();
        if s.selected != "podbag" || s.engine.platform == "linux" {
            return Ok(None);
        }
        Ok(Some(s.vm.clone()))
    }

    fn compression(&self, mode: Option<Mode>) -> Result<Compression, String> {
        let o = &self.opts.compression;
        let (state, started) = {
            let mut s = self.st();
            if s.selected != "podbag" {
                return Err("Image compression requires the podbag engine".into());
            }
            if let (Some(mode), None) = (mode, &o.fail)
                && !o.kept
            {
                s.compression.mode = Some(mode);
            }
            // The command runs in the VM, as the engine's does.
            let started = s.engine.vm.as_deref() == Some("stopped");
            if started {
                Mock::set_vm_running(&mut s, true);
            }
            (s.compression, started.then_some(s.token))
        };
        if let Some(session) = started {
            self.events.send(Event::EngineChanged {
                session,
                action: "start".into(),
            });
        }
        match (&o.fail, mode) {
            (Some(e), Some(_)) => Err(e.clone()),
            _ if o.broken => Ok(Compression::default()),
            _ => Ok(state),
        }
    }

    fn extensions(
        &self,
        action: crate::extensions::Action,
    ) -> Result<Vec<crate::extensions::Installed>, crate::extensions::Failure> {
        let mut state = self.st();
        crate::extensions::update(&mut state.extensions, action)?;
        state.extensions.sort_by(|a, b| {
            (&a.manifest.name, &a.manifest.id).cmp(&(&b.manifest.name, &b.manifest.id))
        });
        Ok(state.extensions.clone())
    }

    fn builder_list(&self, session: u64) -> Result<builders::Catalog, String> {
        let mut state = self.st();
        if state.token != session {
            return Err("The engine has changed".into());
        }
        let context = state.selected.clone();
        if !matches!(context.as_str(), "docker" | "podbag") {
            return Ok(builders::unavailable(builders::Availability::NonLocal));
        }
        let builders = state.builders.entry(context.clone()).or_default();
        Ok(builders.catalog(&context))
    }

    fn builder_inspect(
        &self,
        session: u64,
        scope: &str,
        name: &str,
    ) -> Result<builders::Inspection, String> {
        let catalog = self.builder_list(session)?;
        let builder = catalog
            .builders
            .iter()
            .find(|item| item.name == name && catalog.scope.as_deref() == Some(scope))
            .ok_or("Unknown builder in this context")?;
        let state = self.st();
        let set = state.builders.get(scope).ok_or("The engine has changed")?;
        let running = !builder.managed || set.running;
        Ok(builders::Inspection {
            error: String::new(),
            nodes: vec![builders::Node {
                name: format!("{name}0"),
                endpoint: scope.into(),
                status: if running { "running" } else { "inactive" }.into(),
                buildkit: "v0.33.1".into(),
                platforms: "linux/arm64".into(),
                ..Default::default()
            }],
        })
    }

    fn builder_action(
        &self,
        session: u64,
        scope: &str,
        name: &str,
        action: builders::Action,
    ) -> Result<(), String> {
        let mut state = self.st();
        if state.token != session || state.selected != scope {
            return Err("The engine has changed".into());
        }
        let set = state
            .builders
            .get_mut(scope)
            .ok_or("Unknown builder context")?;
        let managed = !set.removed && name == "local-builder";
        if name != scope && !managed {
            return Err("Unknown builder".into());
        }
        match action {
            builders::Action::Select => set.selected = managed,
            builders::Action::Start if managed => set.running = true,
            builders::Action::Stop if managed => set.running = false,
            builders::Action::Remove if managed => {
                set.removed = true;
                set.selected = false;
            }
            _ => return Err("Only a managed builder can be changed".into()),
        }
        Ok(())
    }

    fn vm_storage(
        &self,
        session: u64,
        edit: Option<crate::VmStorageEdit>,
    ) -> Result<crate::DiskStorage, String> {
        let mut state = self.st();
        if state.token != session || state.selected != "podbag" || state.engine.platform != "macos"
        {
            return Err("VM storage requires the local macOS Podbag engine".into());
        }
        let stopped = state.vm.state == "stopped";
        let disk = &mut state.disk;
        disk.editable = stopped;
        disk.backup_path = None;
        if let Some(edit) = edit {
            if !disk.editable {
                return Err("Stop the VM before changing its storage".into());
            }
            match edit {
                crate::VmStorageEdit::Grow { gib }
                    if gib <= disk.max_size_gib && gib > disk.size_bytes >> 30 =>
                {
                    disk.size_bytes = gib << 30;
                    disk.growth_pending = true;
                    disk.backup_path = Some(disk.path.with_extension("bak"));
                }
                crate::VmStorageEdit::Move { path }
                    if path.starts_with('/') && std::path::Path::new(&path) != disk.path =>
                {
                    disk.backup_path = Some(std::mem::replace(&mut disk.path, path.into()));
                    disk.configured = true;
                }
                _ => return Err("Invalid VM storage change".into()),
            }
        }
        Ok(disk.clone())
    }

    fn engine_config(
        &self,
        session: u64,
        request: crate::EngineConfigRequest,
    ) -> Result<Option<crate::ConfigDocument>, String> {
        use crate::EngineConfigRequest::*;
        let mut state = self.st();
        if state.token != session || state.selected != "podbag" {
            return Err("Engine configuration requires the local Podbag engine".into());
        }
        match request {
            Read => {}
            Check { text } => {
                podbag::config::parse(&text).map_err(|e| e.to_string())?;
                return Ok(None);
            }
            Save { edit } => {
                podbag::config::parse(&edit.text).map_err(|e| e.to_string())?;
                if state.config_text != edit.original {
                    return Err("config.toml changed after it was opened".into());
                }
                state.config_text = Some(edit.text);
            }
        }
        Ok(Some(crate::ConfigDocument {
            path: "/home/podbag/.config/podbag/config.toml".into(),
            text: state.config_text.clone(),
        }))
    }

    fn network_settings(
        &self,
        session: u64,
        edit: Option<(&NetworkPreferences, &str)>,
    ) -> Result<NetworkSettings, String> {
        let mut state = self.st();
        if state.token != session || state.selected != "podbag" {
            return Err("Network settings require the local podbag engine".into());
        }
        let settings = &mut state.network_settings;
        if let Some((values, revision)) = edit {
            if settings.revision != revision {
                return Err("Network settings changed after they were opened".into());
            }
            if settings.preferences != *values {
                settings.preferences = values.clone();
                settings.revision = (revision.parse::<u64>().unwrap() + 1).to_string();
            }
        }
        Ok(settings.clone())
    }

    fn registry_proxy(
        &self,
        session: u64,
        edit: Option<RegistryProxySettings>,
    ) -> Result<RegistryProxySettings, String> {
        let mut state = self.st();
        if state.token != session
            || state.selected != "podbag"
            || !matches!(state.engine.platform, "linux" | "macos")
        {
            return Err("Registry proxy settings require the local Podbag engine".into());
        }
        if let Some(edit) = edit {
            let bytes = serde_json::to_vec(&edit).map_err(|_| "Cannot encode proxy settings")?;
            state.registry_proxy =
                podbag::image::proxy::read_json(bytes.as_slice()).map_err(|error| error.why)?;
        }
        Ok(state.registry_proxy.clone())
    }

    fn vm_set(&self, change: &VmSettings) -> Result<String, String> {
        let mut s = self.st();
        let l = s.vm.limits.unwrap_or(LIMITS);
        let mut next = s.vm.next.unwrap_or_default();
        let mut saved = s.vm.saved.unwrap_or_default();
        if change.cpus.is_none()
            && change.memory_mib.is_none()
            && change.idle_seconds.is_none()
            && change.shared_paths.is_none()
        {
            return Err("nothing to set".into());
        }
        if let Some(paths) = &change.shared_paths {
            self.opts.calls.push(format!("vm_set {}", paths.join(",")));
            if let Some(p) = paths.iter().find(|p| !p.starts_with('/')) {
                return Err(format!("{p} is not an absolute path"));
            }
        }
        if let Some(c) = change.cpus {
            if c < l.cpus_min || c > l.cpus_max {
                return Err(format!(
                    "CPUs must be from {} to {}",
                    l.cpus_min, l.cpus_max
                ));
            }
            next.cpus = Some(c);
            saved.cpus = Some(c);
        }
        if let Some(m) = change.memory_mib {
            if m < l.memory_min_mib || m > l.memory_max_mib {
                return Err(format!(
                    "memory must be from {} to {} MiB",
                    l.memory_min_mib, l.memory_max_mib
                ));
            }
            next.memory_mib = Some(m);
            saved.memory_mib = Some(m);
        }
        if let Some(i) = change.idle_seconds {
            next.idle_seconds = Some(i);
            saved.idle_seconds = Some(i);
        }
        s.vm.next = Some(next);
        s.vm.saved = Some(saved);
        let running = s.vm.state == "running";
        let mut shares = s.vm.shared_paths.clone().unwrap_or_default();
        if let Some(paths) = &change.shared_paths {
            shares.next = paths.clone();
        }
        if running && change.restart {
            s.vm.in_use = Some(next);
            s.vm.uptime_seconds = Some(0);
            shares.in_use = Some(shares.next.clone());
        }
        let shares_pending = running && shares.in_use.as_ref() != Some(&shares.next);
        if s.vm.shared_paths.is_some() {
            s.vm.shared_paths = Some(shares);
        }
        s.vm.pending = running && (s.vm.in_use != Some(next) || shares_pending);
        Ok("Saved.".into())
    }

    /// `add` fails for a name in use, `now` and `remove` for a name not in use.
    fn vm_sync(&self, action: &SyncAction) -> Result<Vec<SyncShare>, String> {
        let (what, name, _) = action.args();
        self.opts.calls.push(
            format!("vm_sync {what} {}", name.unwrap_or_default())
                .trim_end()
                .to_string(),
        );
        let mut s = self.st();
        if s.selected != "podbag" || s.vm.platform != "macos" {
            return Err("Synchronized shares require Podbag on macOS".into());
        }
        let at = name.and_then(|n| s.sync.iter().position(|share| share.name == n));
        match action {
            SyncAction::List => {}
            SyncAction::Add { name, path } => {
                if at.is_some() {
                    return Err("Share already exists".into());
                }
                s.sync.push(sample_share(name, path));
            }
            SyncAction::Now(_) | SyncAction::Remove(_) => {
                let Some(at) = at else {
                    return Err("Share not found".into());
                };
                if matches!(action, SyncAction::Remove(_)) {
                    s.sync.remove(at);
                }
            }
        }
        drop(s);
        if !matches!(action, SyncAction::List) {
            self.wait(300);
        }
        Ok(self.st().sync.clone())
    }

    fn refresh(&self, _asked: bool) {}

    fn kube_contexts(&self) -> Result<Vec<String>, String> {
        Ok(vec!["dev".into(), "staging".into(), "ocp".into()])
    }

    /// Routes count only on OpenShift (`ocp`).
    fn kube_counts(
        &self,
        context: Option<&str>,
        all_namespaces: bool,
    ) -> Result<BTreeMap<String, usize>, String> {
        let mut kinds = vec![
            Resource::Pods,
            Resource::Services,
            Resource::Deployments,
            Resource::Claims,
        ];
        if mock_openshift(context) {
            kinds.push(Resource::Routes);
        }
        kinds
            .into_iter()
            .map(|kind| {
                let n = self.kube_items(kind, all_namespaces, context)?.len();
                Ok((kind.key().to_string(), n))
            })
            .collect()
    }

    fn kube_list(
        &self,
        kind: Resource,
        all_namespaces: bool,
        context: Option<&str>,
    ) -> Result<Snapshot, String> {
        let context = context.filter(|c| !c.is_empty()).unwrap_or("dev");
        self.opts
            .calls
            .push(format!("kube_list {} {context}", kind.key()));
        Ok(Snapshot {
            context: context.into(),
            namespace: "default".into(),
            items: self.kube_items(kind, all_namespaces, Some(context))?,
        })
    }

    fn kube_openshift(&self, context: Option<&str>) -> Result<bool, String> {
        Ok(mock_openshift(context))
    }

    /// Nothing opens: the address goes to [`Options::calls`].
    fn kube_open_route(&self, context: &str, namespace: &str, name: &str) -> Result<(), String> {
        let routes = self.kube_items(Resource::Routes, true, Some(context))?;
        let route = routes
            .iter()
            .find(|r| r["metadata"]["namespace"] == namespace && r["metadata"]["name"] == name)
            .ok_or_else(|| {
                format!(
                    "kubectl (exit status: 1): Error from server (NotFound): routes.route.openshift.io \"{name}\" not found"
                )
            })?;
        let url = route_url(route)?;
        self.opts.calls.push(format!(
            "kube_open_route {context} {namespace} {name} {url}"
        ));
        Ok(())
    }

    fn migration_sources(&self) -> Vec<String> {
        ["docker", "podman", "kiri", "podbag"]
            .map(String::from)
            .into()
    }

    fn migration_list(
        &self,
        _source: &str,
        kind: Kind,
        hosts: &[(String, String)],
    ) -> Result<Listing, String> {
        let list = self.list(kind)?;
        if !matches!(kind, Kind::Images | Kind::Containers) {
            return Ok(Listing {
                list,
                registries: BTreeMap::new(),
            });
        }
        let mut rows = serde_json::to_value(&list).map_err(|e| e.to_string())?;
        if let (Some(rows), Value::Array(more)) = (rows.as_array_mut(), migration_images(self.now))
            && kind == Kind::Images
        {
            rows.extend(more);
        }
        migration::listing(kind, rows, hosts)
    }

    /// `prepare`, `image` or `files`, `verify`: 180 ms apart.
    fn migrate(&self, request: &Request, progress: &dyn Fn(&str)) -> Result<(), String> {
        let middle = if request.kind == "images" {
            "image"
        } else {
            "files"
        };
        for step in ["prepare", middle, "verify"] {
            progress(step);
            self.wait(180);
        }
        Ok(())
    }

    /// Start fails when Ollama is `missing`; a run waits for the gate.
    fn models(&self, action: model_runner::Action) -> Result<Reply, Failure> {
        use model_runner::Action;
        self.opts.calls.push(format!("models {}", action.name()));
        let mut s = self.st();
        match &action {
            Action::Status => {}
            Action::Start => {
                if self.opts.models.missing {
                    return Err(Failure::new("missing"));
                }
                s.model_runner = true;
            }
            Action::Stop => {
                s.model_runner = false;
                s.models.iter_mut().for_each(|m| m.loaded = false);
            }
            _ if !s.model_runner => return Err(Failure::new("stopped")),
            Action::Pull { model } => {
                model_runner::registry_model(model)?;
                let name = model.trim();
                if !s.models.iter().any(|m| m.name == name) {
                    s.models.push(Model {
                        name: name.into(),
                        size: DEMO_MODEL.1,
                        loaded: false,
                    });
                }
            }
            Action::Run { model, prompt } => {
                model_runner::check_prompt(prompt)?;
                if !s.models.iter().any(|m| m.name == *model) {
                    return Err(Failure::new("notInstalled"));
                }
                drop(s);
                self.opts.models.gate.pass();
                let mut s = self.st();
                if !s.model_runner {
                    return Err(Failure::new("stopped"));
                }
                if let Some(m) = s.models.iter_mut().find(|m| m.name == *model) {
                    m.loaded = true;
                }
                return Ok(Reply::Generation(Generation {
                    response: "Blue.".into(),
                    done: true,
                }));
            }
            Action::Unload { model } => {
                let Some(m) = s.models.iter_mut().find(|m| m.name == *model) else {
                    return Err(Failure::new("notInstalled"));
                };
                m.loaded = false;
            }
        }
        let running = s.model_runner;
        Ok(Reply::Status(model_runner::Status {
            running,
            version: if running {
                "0.40.17".into()
            } else {
                String::new()
            },
            models: if running {
                s.models.clone()
            } else {
                Vec::new()
            },
        }))
    }

    fn settings(&self) -> Settings {
        self.st().settings.clone()
    }

    fn set_settings(&self, settings: Settings) -> Result<(), String> {
        self.st().settings = settings.checked();
        self.events.send(Event::StatusChanged);
        Ok(())
    }

    fn language(&self) -> &'static str {
        strings::resolve(&self.st().settings.language, sys_locale::get_locale())
    }

    fn take_star_prompt(&self) -> bool {
        std::mem::take(&mut self.st().star_prompt)
    }

    /// A packaged app with a separate `podbag` for the terminal. A check now finds the
    /// newer release (and later reads remember it).
    fn updates(&self, check: Check) -> Updates {
        let newer = {
            let mut s = self.st();
            s.update_found |= self.opts.update || check == Check::Now;
            s.update_found
        };
        let latest = if newer { "0.2.0" } else { "0.1.0" };
        Updates {
            engine: Some("0.1.0".into()),
            app: "0.1.0".into(),
            latest: Some(latest.into()),
            newer,
            checked: Some(self.now.max(3600) as u64 - 3600),
            error: None,
            installer: Some("macos-app".into()),
            terminal: Some(TERMINAL_LINK.into()),
            terminal_version: Some("0.1.0".into()),
            app_newer: newer,
            cli_newer: newer,
            terminal_older: false,
        }
    }

    /// The upgrade needs an administrator.
    fn upgrade(&self, _version: Option<&str>) -> Upgraded {
        Upgraded {
            ok: false,
            code: podbag::dashboard::cli::NEEDS_ADMIN,
            output: "podbag: cannot write to /usr/local/bin: it needs administrator rights".into(),
            needs_admin: true,
            command: Some(format!("sudo {TERMINAL_LINK} upgrade")),
        }
    }

    fn update_app(&self, _version: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    fn terminal_command(&self) -> Option<terminal_cli::Status> {
        Some(self.st().terminal.clone())
    }

    /// The command is made after 650 ms.
    fn install_terminal_command(&self) -> Result<Option<terminal_cli::Status>, String> {
        self.wait(650);
        let mut s = self.st();
        s.terminal.state = "app";
        s.terminal.path = Some(s.terminal.target.clone());
        Ok(Some(s.terminal.clone()))
    }

    fn open_releases(&self) -> Result<(), String> {
        Ok(())
    }

    fn open_repository(&self) -> Result<(), String> {
        Ok(())
    }

    fn open_training_guide(&self, _id: &str, _lang: &str) -> Result<(), String> {
        Ok(())
    }

    fn launch_at_login(&self) -> Result<bool, String> {
        Ok(self.login.load(Ordering::SeqCst))
    }

    fn set_launch_at_login(&self, on: bool) -> Result<(), String> {
        self.login.store(on, Ordering::SeqCst);
        Ok(())
    }

    fn tray(&self) -> TrayModel {
        let language = self.language();
        let s = self.st();
        let engine = EngineState::of(&s.engine);
        let running = matches!(engine, EngineState::Ready | EngineState::VmRunning)
            .then(|| s.containers.iter().filter(|c| c.state == "running").count());
        TrayModel::new(&TrayInput {
            language,
            engine,
            running,
            engine_name: Choice::parse(&s.selected).map_or(s.selected.clone(), |c| c.label()),
            uses_podbag: s.selected == "podbag",
            update: self.opts.update.then(|| "0.2.0".into()),
            docker: Some(s.docker.clone()),
        })
    }

    fn set_visible(&self, _visible: bool) {}

    fn toggle_vm(&self) -> Result<(), String> {
        let running = self.st().engine.vm.as_deref() == Some("running");
        self.vm(if running { "stop" } else { "start" })
    }

    /// The model runner stops, as the real one does.
    fn shutdown(&self) {
        let mut s = self.st();
        s.model_runner = false;
        s.models.iter_mut().for_each(|m| m.loaded = false);
    }

    fn training_modules(
        &self,
        action: &str,
        package: Option<&str>,
        id: Option<&str>,
    ) -> Result<Vec<crate::training::LessonRecord>, String> {
        lock(&self.training).modules(action, package, id)
    }

    /// Also `start` (with the module id as `name`) and `advance`, which stand in for
    /// the terminal: see [`crate::mock_training`].
    fn practice(
        &self,
        action: &str,
        session: Option<&str>,
        name: Option<&str>,
        containers: bool,
        course: bool,
    ) -> Result<Value, String> {
        lock(&self.training).practice(action, session, name, containers, course)
    }

    fn tutorial_lab(
        &self,
        action: &str,
        value: Option<&str>,
    ) -> Result<podbag::dashboard::lab::Snapshot, String> {
        lock(&self.training).tutorial_lab(action, value)
    }

    fn saved_lesson(&self) -> Option<Value> {
        lock(&self.training).saved_lesson()
    }

    fn save_lesson(&self, lesson: Option<&Value>) -> Result<(), String> {
        lock(&self.training).save_lesson(lesson);
        Ok(())
    }
}

/// The module file of a training module, for previews (see `mock_training`).
pub use crate::mock_training::sample_module;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Backend;
    use std::sync::mpsc::Receiver;

    fn backend(opts: Options) -> (Backend, Receiver<Event>) {
        Backend::mock(
            Options {
                instant: true,
                ..opts
            },
            || {},
        )
    }

    #[test]
    fn the_sample_data_is_that_of_the_preview() {
        let (b, _) = backend(Options::default());
        assert_eq!(b.connection().name, "podbag");
        assert_eq!(b.engine(false).engine_name, "podbag");
        let list = b.containers().unwrap();
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].name(), "web");
        assert_eq!(list[0].ports_text(), "8080->80/tcp");
        assert_eq!(list[2].ports_text(), "6379/tcp");
        assert_eq!(list[3].state, "exited");
        let volumes = b.volumes().unwrap();
        assert!(volumes[0].created_at.ends_with(".000Z"), "{volumes:?}");
        assert_eq!(b.list(Kind::Networks).unwrap().len(), 4);
        assert_eq!(
            b.prune(Kind::Images).unwrap(),
            Pruned {
                count: 2,
                bytes: 61_000_000
            }
        );
        assert_eq!(b.container_json("x", "top"), Err("not supported".into()));
        assert_eq!(b.search("nginx/").results[0].name, "nginx/nginx-ingress");
        let acr = b.registries(&[("acr".into(), "contoso.azurecr.io".into())]);
        let acr = acr.iter().find(|r| r.id == "acr").unwrap();
        assert_eq!(acr.hint.as_deref(), Some("az acr login --name contoso"));
        let counts = b.kube_counts(None, false).unwrap();
        assert_eq!(counts.values().sum::<usize>(), 4);
        assert_eq!(b.kube_counts(None, true).unwrap()["pods"], 2);
        let pods = b.kube_list(Resource::Pods, true, None).unwrap();
        assert_eq!(pods.items[1]["metadata"]["namespace"], "staging");
        let vm = b.vm_status().unwrap().unwrap();
        assert_eq!(vm.next.unwrap().memory_mib, Some(6144));
        assert!(vm.pending);
        assert_eq!(
            serde_json::to_value(&vm).unwrap()["inUse"]["memoryMiB"],
            4096
        );
    }

    #[test]
    fn a_run_pulls_a_missing_image_then_adds_a_running_container() {
        let calls = Calls::default();
        let (b, rx) = backend(Options {
            calls: calls.clone(),
            ..Options::default()
        });
        let images = b.images().unwrap().len();
        let spec = RunSpec {
            image: "busybox:1.37".into(),
            command: vec!["sleep".into(), "60".into()],
            ports: vec![podbag::client::PublishedPort {
                host: 0,
                container: 53,
                protocol: "udp".into(),
            }],
            ..RunSpec::default()
        };
        let id = b.run_container(&spec).unwrap();
        let lines: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::PullProgress { line, .. } => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            [
                "Pulling from library/busybox:1.37",
                "Download complete",
                "Pull complete"
            ]
        );
        assert_eq!(b.images().unwrap().len(), images + 1);
        let c = &b.containers().unwrap()[0];
        assert_eq!((c.id.as_str(), c.name()), (id.as_str(), "eager_tesla"));
        assert_eq!(
            (c.state.as_str(), c.command.as_str()),
            ("running", "sleep 60")
        );
        assert_eq!(c.ports_text(), "53/udp");
        assert_eq!(calls.of("run_container").len(), 1);
        // An image it has runs at once; a missing image and a bad spec fail.
        let web = RunSpec {
            image: "nginx:alpine".into(),
            name: "site".into(),
            ..RunSpec::default()
        };
        b.run_container(&web).unwrap();
        assert!(
            !rx.try_iter()
                .any(|e| matches!(e, Event::PullProgress { .. }))
        );
        let missing = RunSpec {
            image: "notfound/app".into(),
            ..RunSpec::default()
        };
        let e = b.run_container(&missing).unwrap_err();
        assert_eq!(e, "notfound/app:latest: manifest unknown");
        let bad = RunSpec {
            env: vec!["NOPE".into()],
            ..RunSpec::default()
        };
        assert!(b.run_container(&bad).is_err());
    }

    #[test]
    fn only_the_openshift_context_has_routes() {
        let calls = Calls::default();
        let (b, _) = backend(Options {
            calls: calls.clone(),
            ..Options::default()
        });
        assert_eq!(b.kube_contexts().unwrap(), ["dev", "staging", "ocp"]);
        assert!(!b.kube_openshift(None).unwrap());
        assert!(b.kube_openshift(Some("ocp")).unwrap());
        assert!(
            !b.kube_counts(Some("dev"), false)
                .unwrap()
                .contains_key("routes")
        );
        let counts = b.kube_counts(Some("ocp"), false).unwrap();
        assert_eq!(counts["routes"], 2);
        assert_eq!(b.kube_counts(Some("ocp"), true).unwrap()["routes"], 3);
        let e = b.kube_list(Resource::Routes, false, Some("dev")).err();
        assert!(e.unwrap().contains("doesn't have a resource type"));
        let routes = b.kube_list(Resource::Routes, false, Some("ocp")).unwrap();
        assert_eq!(routes.items[0]["spec"]["host"], "shop.apps.ocp.example.com");
        b.kube_open_route("ocp", "default", "shop").unwrap();
        assert!(b.kube_open_route("ocp", "default", "docs").is_err());
        assert!(b.kube_open_route("dev", "default", "shop").is_err());
        assert_eq!(
            calls.of("kube_open_route"),
            ["ocp default shop https://shop.apps.ocp.example.com/cart"]
        );
        assert_eq!(calls.of("kube_list"), ["routes dev", "routes ocp"]);
    }

    #[test]
    fn rows_make_long_lists() {
        let (b, _) = backend(Options {
            rows: Some(1000),
            ..Options::default()
        });
        let list = b.containers().unwrap();
        assert_eq!(list.len(), 1000);
        assert_eq!(list[4].name(), "worker-00004");
        assert_eq!(list[0].state, "exited");
        assert_eq!(list[1].ports_text(), "8001->8080/tcp");
    }

    #[test]
    fn actions_change_the_lists_and_send_events() {
        let (b, rx) = backend(Options::default());
        b.act("d4e5f6a7b8c9102b", "start").unwrap();
        assert_eq!(
            rx.try_recv(),
            Ok(Event::Changed {
                session: 1,
                kind: "container".into()
            })
        );
        assert_eq!(b.containers().unwrap()[3].state, "running");
        assert!(
            b.remove(Kind::Containers, "a1b2c3d4e5f60718", false)
                .is_err()
        );
        b.remove(Kind::Containers, "a1b2c3d4e5f60718", true)
            .unwrap();
        assert_eq!(b.containers().unwrap().len(), 4);
        assert!(b.remove(Kind::Networks, "4e1c0b8a2f6d9e7c", false).is_err());
        b.rename("b2c3d4e5f6a70819", "db2").unwrap();
        assert!(b.rename("c3d4e5f6a7b8091a", "db2").is_err());
        b.create_volume("cache-data").unwrap();
        assert!(b.create_volume("cache-data").is_err());
        let _: Vec<Event> = rx.try_iter().collect();

        assert_eq!(b.logs_start("x"), Ok(1));
        match rx.try_recv() {
            Ok(Event::Logs {
                generation: 1,
                lines,
                ..
            }) => assert_eq!(lines.len(), 28),
            other => panic!("{other:?}"),
        }
        b.watch(true);
        assert_eq!(
            rx.try_recv(),
            Ok(Event::Live {
                session: 1,
                on: true
            })
        );

        b.vm("stop").unwrap();
        assert_eq!(
            rx.try_recv(),
            Ok(Event::EngineChanged {
                session: 1,
                action: "stop".into()
            })
        );
        assert!(!b.engine(false).reachable);
        assert!(b.engine(true).reachable);
    }

    #[test]
    fn background_work_reports_its_end() {
        let (b, rx) = backend(Options::default());
        let wait = |want: fn(&Event) -> bool| {
            loop {
                let e = rx.recv_timeout(Duration::from_secs(5)).expect("no event");
                if want(&e) {
                    return e;
                }
            }
        };
        b.start_pull("nginx", Some("dockerhub"), None);
        let done = wait(|e| matches!(e, Event::PullDone { .. }));
        assert_eq!(
            done,
            Event::PullDone {
                session: 1,
                result: Ok("nginx".into())
            }
        );
        assert!(
            b.images()
                .unwrap()
                .iter()
                .any(|i| i.repo_tags == ["nginx:latest"])
        );
        b.start_pull("me/notfound", Some("github"), None);
        let failed = wait(|e| matches!(e, Event::PullDone { .. }));
        assert!(
            matches!(failed, Event::PullDone { result: Err(ref e), .. } if e.contains("ghcr.io/me/notfound")),
            "{failed:?}"
        );

        b.start_docker_switch("back");
        assert_eq!(
            wait(|e| matches!(e, Event::DockerSwitching(_))),
            Event::DockerSwitching("back".into())
        );
        let Event::DockerSwitched(Ok(now)) = wait(|e| matches!(e, Event::DockerSwitched(_))) else {
            panic!()
        };
        assert!(!now.is_podbag);
        assert_eq!(b.docker_command(), now);

        b.start_migration(Request {
            kind: "images".into(),
            ..Request::default()
        });
        let mut steps = Vec::new();
        loop {
            match wait(|e| matches!(e, Event::MigrationProgress(_) | Event::MigrationDone(_))) {
                Event::MigrationProgress(step) => steps.push(step),
                done => {
                    assert_eq!(done, Event::MigrationDone(Ok(())));
                    break;
                }
            }
        }
        assert_eq!(steps, ["prepare", "image", "verify"]);

        b.start_vm("stop");
        assert!(matches!(
            wait(|e| matches!(e, Event::VmDone { .. })),
            Event::VmDone { result: Ok(()), .. }
        ));
        b.start_engine(true);
        let Event::Engine { status, .. } = wait(|e| matches!(e, Event::Engine { .. })) else {
            panic!()
        };
        assert!(status.reachable);
    }

    #[test]
    fn another_engine_has_a_new_token_and_no_vm() {
        let (b, _) = backend(Options::default());
        assert!(b.set_engine("no such engine").is_err());
        b.set_engine("kiri").unwrap();
        let c = b.connection();
        assert_eq!((c.token, c.name.as_str(), c.local), (2, "kiri", false));
        let e = b.engine(false);
        assert_eq!((e.engine, e.engine_name.as_str()), ("kiri", "Kiri"));
        assert_eq!(b.vm_status(), Ok(None));
        assert_eq!(b.vm("stop"), Err("VM controls require Podbag".into()));
        let (docker, _) = backend(Options {
            scenario: Scenario::Docker,
            ..Options::default()
        });
        assert_eq!(docker.connection().name, "docker");
        assert_eq!(docker.vm_status(), Ok(None));
    }

    #[test]
    fn vm_settings_are_kept_within_the_limits() {
        let (b, _) = backend(Options {
            scenario: Scenario::VmStopped,
            ..Options::default()
        });
        assert!(!b.engine(false).reachable);
        let set = |cpus, memory_mib| VmSettings {
            cpus,
            memory_mib,
            ..VmSettings::default()
        };
        assert!(b.vm_set(&set(Some(64), None)).is_err());
        assert!(b.vm_set(&set(None, None)).is_err());
        assert_eq!(b.vm_set(&set(Some(2), Some(2048))), Ok("Saved.".into()));
        let vm = b.vm_status().unwrap().unwrap();
        assert_eq!(vm.next.unwrap().cpus, Some(2));
        assert!(!vm.pending, "a stopped VM has nothing pending");
        b.vm("start").unwrap();
        assert_eq!(
            b.vm_status().unwrap().unwrap().in_use.unwrap().memory_mib,
            Some(2048)
        );
    }

    #[test]
    fn image_compression_is_set_in_the_vm_and_its_cases_show() {
        let (b, rx) = backend(Options {
            scenario: Scenario::VmStopped,
            compression_prompt: true,
            ..Options::default()
        });
        assert!(!b.settings().compression_prompt_seen);
        let alpine = b.images().unwrap().pop().unwrap();
        assert_eq!(
            alpine.podbag_compression.map(|c| c.size),
            Some(3_900_000),
            "the sample has a compressed image"
        );
        // A change starts the stopped VM, as the engine's command does.
        let on = b.compression(Some(Mode::Idle)).unwrap();
        assert_eq!((on.mode, on.after), (Some(Mode::Idle), Some(604_800)));
        assert!(b.engine(false).reachable);
        assert!(
            rx.try_iter()
                .any(|e| matches!(e, Event::EngineChanged { .. }))
        );
        assert!(b.compression(None).unwrap().is_on());
        // An environment setting, a reply without a mode, a failed change.
        let kept = |o: CompressionOptions| {
            backend(Options {
                compression: o,
                ..Options::default()
            })
            .0
        };
        let b = kept(CompressionOptions {
            kept: true,
            ..CompressionOptions::default()
        });
        assert_eq!(
            b.compression(Some(Mode::Idle)).unwrap().mode,
            Some(Mode::Off)
        );
        let b = kept(CompressionOptions {
            broken: true,
            ..CompressionOptions::default()
        });
        assert_eq!(b.compression(None).unwrap().mode, None);
        let b = kept(CompressionOptions {
            fail: Some("disk is full".into()),
            vm_error: true,
            ..CompressionOptions::default()
        });
        assert_eq!(b.compression(Some(Mode::Idle)), Err("disk is full".into()));
        assert!(b.vm_status().is_err());
        assert!(!b.compression(None).unwrap().is_on());
        // Another engine has none.
        b.set_engine("docker").unwrap();
        assert!(b.compression(None).is_err());
    }

    #[test]
    fn settings_and_the_terminal_command() {
        let (b, _) = backend(Options {
            language: Some("ko".into()),
            first_launch: true,
            update: true,
            ..Options::default()
        });
        assert_eq!(b.language(), "ko");
        assert!(b.take_star_prompt());
        assert!(!b.take_star_prompt());
        b.set_settings(Settings {
            language: "xx".into(),
            ..Settings::default()
        })
        .unwrap();
        assert_eq!(b.settings().language, "system");
        assert!(b.updates(Check::Never).newer);
        assert!(b.upgrade(None).needs_admin);
        assert_eq!(b.terminal_command().unwrap().state, "other");
        assert_eq!(b.install_terminal_command().unwrap().unwrap().state, "app");
    }

    #[test]
    fn iso_times_are_those_of_javascript() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_795_000_000), "2026-11-18T11:06:40.000Z");
        assert_eq!(iso_time(951_782_400), "2000-02-29T00:00:00.000Z");
    }
}
