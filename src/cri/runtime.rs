//! The CRI runtime service: pod sandboxes and their containers, on podbag's containers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::grpc::{Code, Status};
use super::mapping::{self, Found, PodView};
use super::pb;
use super::pod::{self, Wanted};
use super::records::{self, Records, now_ns};
use super::service::{Options, status_of};
use crate::containers::{ContainerHandle, ExecOptions};
use crate::paths::Dirs;
use crate::state::{self, Status as RunStatus};
use crate::{Error, ErrorKind};

type Answer<T> = std::result::Result<T, Status>;

/// The most output ExecSync keeps of each stream, as containerd does.
const EXEC_OUTPUT_LIMIT: usize = 16 << 20;

/// The pods and containers of `podbag cri serve`.
pub struct Runtime {
    pub options: Options,
    dirs: Dirs,
    records: Records,
    /// `<runtime>/cri/ns/<sandbox>`: what each pod holds.
    ns_root: PathBuf,
    /// One lock per pod or container, for the calls that change it.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub events: super::events::Events,
}

fn err(e: Error) -> Status {
    status_of(&e)
}

impl Runtime {
    pub fn new(options: Options) -> crate::Result<Runtime> {
        let dirs = crate::paths::dirs()?;
        let records = Records::new(dirs.data.join("cri"));
        let ns_root = dirs.runtime.join("cri/ns");
        Ok(Runtime {
            options,
            records,
            ns_root,
            dirs,
            locks: Mutex::new(HashMap::new()),
            events: super::events::Events::default(),
        })
    }

    fn lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut map = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        // Locks of removed objects go once nobody holds them.
        map.retain(|_, l| Arc::strong_count(l) > 1);
        Arc::clone(map.entry(id.to_string()).or_default())
    }

    fn sandbox_id(&self, given: &str) -> Answer<String> {
        Records::resolve(&self.records.sandbox_ids(), given, "pod sandbox").map_err(err)
    }

    fn container_id(&self, given: &str) -> Answer<String> {
        Records::resolve(&self.records.container_ids(), given, "container").map_err(err)
    }

    // ----- Pod sandboxes -----

    pub fn run_pod_sandbox(&self, config: pb::PodSandboxConfig) -> Answer<String> {
        let meta = config.metadata.clone().unwrap_or_default();
        if meta.name.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "the pod has no name"));
        }
        // One sandbox per pod and attempt.
        for other in self.records.sandbox_ids() {
            if let Ok((_, c)) = self.records.sandbox(&other) {
                if c.metadata.as_ref() == Some(&meta) {
                    return Err(Status::new(
                        Code::AlreadyExists,
                        format!(
                            "pod sandbox {}_{}_{}_{} exists already: {other}",
                            meta.name, meta.namespace, meta.uid, meta.attempt
                        ),
                    ));
                }
            }
        }
        let id = crate::id::new_id().map_err(err)?;
        let linux = config.linux.clone().unwrap_or_default();
        let ns = linux
            .security_context
            .as_ref()
            .and_then(|s| s.namespace_options.clone())
            .unwrap_or_default();
        let node = pb::NamespaceMode::Node as i32;
        let host_net = ns.network == node;
        let wanted = Wanted {
            net: !host_net,
            ipc: ns.ipc != node,
            // The host's network comes with the host's name.
            uts: !host_net,
        };
        let hostname = if config.hostname.is_empty() {
            rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned()
        } else {
            config.hostname.clone()
        };
        let mut sysctls: Vec<(String, String)> = linux
            .sysctls
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        sysctls.sort();
        let ns_dir = self.ns_root.join(&id);
        let held = pod::hold(&ns_dir, wanted, &hostname, &sysctls).map_err(err)?;
        let mut sandbox = records::Sandbox {
            id: id.clone(),
            created: now_ns(),
            ready: true,
            net: held.net.as_ref().map(|p| p.display().to_string()),
            ipc: held.ipc.as_ref().map(|p| p.display().to_string()),
            uts: held.uts.as_ref().map(|p| p.display().to_string()),
            shm: held.shm.as_ref().map(|p| p.display().to_string()),
            ..Default::default()
        };
        let undo = |s: &records::Sandbox| {
            if let Some(pid) = s.pid_holder {
                pod::stop_pid(pid);
            }
            pod::release(&ns_dir);
            let _ = self.records.remove_sandbox(&id);
        };
        if ns.pid == pb::NamespaceMode::Pod as i32 {
            match pod::hold_pid(&ns_dir) {
                Ok(pid) => {
                    sandbox.pid_holder = Some(pid);
                    sandbox.pid = Some(ns_dir.join("pid").display().to_string());
                }
                Err(e) => {
                    undo(&sandbox);
                    return Err(err(e));
                }
            }
        }
        // The pod's own files, mounted into its containers.
        let files = self.records.sandbox_dir(&id);
        let host_resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let written = std::fs::create_dir_all(&files)
            .and_then(|()| std::fs::write(files.join("hostname"), format!("{hostname}\n")))
            .and_then(|()| {
                std::fs::write(
                    files.join("resolv.conf"),
                    mapping::resolv_conf(config.dns_config.as_ref(), &host_resolv),
                )
            });
        if let Err(e) = written {
            undo(&sandbox);
            return Err(err(Error::io("cannot write the pod's files", &e, "")));
        }
        if let Some(netns) = &sandbox.net {
            match self.network_add(&id, &config, netns) {
                Ok((file, result)) => {
                    sandbox.ips = super::cni::ips(&result);
                    sandbox.cni_file = Some(file);
                    sandbox.cni_result = Some(result);
                }
                Err(e) => {
                    undo(&sandbox);
                    return Err(err(e));
                }
            }
        }
        if let Err(e) = self.records.save_sandbox(&sandbox, Some(&config)) {
            if let (Some(netns), Some(file)) = (&sandbox.net, &sandbox.cni_file) {
                let _ = self.network_del(&id, &config, netns, file, sandbox.cni_result.as_ref());
            }
            undo(&sandbox);
            return Err(err(e));
        }
        Ok(id)
    }

    fn attachment(
        &self,
        id: &str,
        config: &pb::PodSandboxConfig,
        netns: &str,
    ) -> super::cni::Attachment {
        let meta = config.metadata.clone().unwrap_or_default();
        super::cni::Attachment {
            container_id: id.to_string(),
            netns: netns.to_string(),
            ifname: "eth0".into(),
            args: super::cni::pod_args(&meta.namespace, &meta.name, id, &meta.uid),
            ports: config
                .port_mappings
                .iter()
                .map(|p| super::cni::PortMapping {
                    host_port: p.host_port,
                    container_port: p.container_port,
                    protocol: match pb::Protocol::try_from(p.protocol) {
                        Ok(pb::Protocol::Udp) => "udp",
                        Ok(pb::Protocol::Sctp) => "sctp",
                        _ => "tcp",
                    }
                    .into(),
                    host_ip: p.host_ip.clone(),
                })
                .collect(),
        }
    }

    fn network_add(
        &self,
        id: &str,
        config: &pb::PodSandboxConfig,
        netns: &str,
    ) -> crate::Result<(String, serde_json::Value)> {
        let conf = super::cni::load(&self.options.cni_conf_dir)?;
        let a = self.attachment(id, config, netns);
        let result = super::cni::add(&conf, &a, &self.options.cni_bin_dirs)?;
        Ok((conf.file.display().to_string(), result))
    }

    fn network_del(
        &self,
        id: &str,
        config: &pb::PodSandboxConfig,
        netns: &str,
        file: &str,
        result: Option<&serde_json::Value>,
    ) -> crate::Result<()> {
        // The configuration the network was made with; the current one when that is gone.
        let conf = match std::fs::read_to_string(file) {
            Ok(text) => super::cni::parse(&text, Path::new(file))?,
            Err(_) => super::cni::load(&self.options.cni_conf_dir)?,
        };
        let a = self.attachment(id, config, netns);
        super::cni::del(&conf, &a, result, &self.options.cni_bin_dirs)
    }

    pub fn stop_pod_sandbox(&self, given: &str) -> Answer<()> {
        // Stopping what is gone already succeeds.
        let Ok(id) = self.sandbox_id(given) else {
            return Ok(());
        };
        let lock = self.lock(&id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut sandbox, config) = self.records.sandbox(&id).map_err(err)?;
        for c in self.containers_of(&id) {
            self.stop_container_now(&c, Duration::ZERO)?;
        }
        if let (Some(netns), Some(file)) = (sandbox.net.clone(), sandbox.cni_file.clone()) {
            self.network_del(&id, &config, &netns, &file, sandbox.cni_result.as_ref())
                .map_err(err)?;
            sandbox.cni_file = None;
            sandbox.cni_result = None;
        }
        if let Some(pid) = sandbox.pid_holder.take() {
            pod::stop_pid(pid);
            sandbox.pid = None;
        }
        pod::release_net(&self.ns_root.join(&id));
        sandbox.net = None;
        sandbox.ready = false;
        self.records.save_sandbox(&sandbox, None).map_err(err)
    }

    pub fn remove_pod_sandbox(&self, given: &str) -> Answer<()> {
        let Ok(id) = self.sandbox_id(given) else {
            return Ok(());
        };
        self.stop_pod_sandbox(&id)?;
        let lock = self.lock(&id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        for c in self.containers_of(&id) {
            self.remove_container(&c)?;
        }
        pod::release(&self.ns_root.join(&id));
        self.records.remove_sandbox(&id).map_err(err)
    }

    /// The IDs of a pod's containers.
    fn containers_of(&self, sandbox: &str) -> Vec<String> {
        self.records
            .container_ids()
            .into_iter()
            .filter(|c| {
                self.records
                    .container(c)
                    .is_ok_and(|(r, _)| r.sandbox == sandbox)
            })
            .collect()
    }

    pub fn pod_sandbox_status(
        &self,
        given: &str,
        verbose: bool,
    ) -> Answer<pb::PodSandboxStatusResponse> {
        let id = self.sandbox_id(given)?;
        let (s, config) = self.records.sandbox(&id).map_err(err)?;
        let ns = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|s| s.namespace_options.clone())
            .unwrap_or_default();
        let mut info = HashMap::new();
        if verbose {
            info.insert(
                "info".to_string(),
                serde_json::json!({
                    "netns": s.net,
                    "ipc": s.ipc,
                    "uts": s.uts,
                    "pidHolder": s.pid_holder,
                    "cniFile": s.cni_file,
                    "cniResult": s.cni_result,
                })
                .to_string(),
            );
        }
        Ok(pb::PodSandboxStatusResponse {
            status: Some(pb::PodSandboxStatus {
                id: s.id.clone(),
                metadata: config.metadata.clone(),
                state: sandbox_state(&s),
                created_at: s.created,
                network: Some(pb::PodSandboxNetworkStatus {
                    ip: s.ips.first().cloned().unwrap_or_default(),
                    additional_ips: s
                        .ips
                        .iter()
                        .skip(1)
                        .map(|ip| pb::PodIp { ip: ip.clone() })
                        .collect(),
                }),
                linux: Some(pb::LinuxPodSandboxStatus {
                    namespaces: Some(pb::Namespace {
                        options: Some(pb::NamespaceOption {
                            network: ns.network,
                            pid: ns.pid,
                            ipc: ns.ipc,
                            ..Default::default()
                        }),
                    }),
                }),
                labels: config.labels.clone(),
                annotations: config.annotations.clone(),
                runtime_handler: String::new(),
            }),
            info,
            containers_statuses: Vec::new(),
            timestamp: now_ns(),
        })
    }

    pub fn list_pod_sandbox(
        &self,
        filter: Option<pb::PodSandboxFilter>,
    ) -> Answer<Vec<pb::PodSandbox>> {
        let filter = filter.unwrap_or_default();
        let mut out = Vec::new();
        for id in self.records.sandbox_ids() {
            if !id.starts_with(&filter.id) {
                continue;
            }
            let Ok((s, config)) = self.records.sandbox(&id) else {
                continue;
            };
            let state = sandbox_state(&s);
            if filter.state.as_ref().is_some_and(|f| f.state != state) {
                continue;
            }
            if !mapping::matches_labels(&config.labels, &filter.label_selector) {
                continue;
            }
            out.push(pb::PodSandbox {
                id: s.id,
                metadata: config.metadata,
                state,
                created_at: s.created,
                labels: config.labels,
                annotations: config.annotations,
                runtime_handler: String::new(),
            });
        }
        Ok(out)
    }

    // ----- Containers -----

    pub fn create_container(
        &self,
        sandbox_given: &str,
        config: pb::ContainerConfig,
    ) -> Answer<String> {
        let sandbox_id = self.sandbox_id(sandbox_given)?;
        let lock = self.lock(&sandbox_id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (sandbox, pod_config) = self.records.sandbox(&sandbox_id).map_err(err)?;
        if !sandbox.ready {
            return Err(Status::new(
                Code::FailedPrecondition,
                format!("pod sandbox {sandbox_id} is not ready"),
            ));
        }
        let meta = config.metadata.clone().unwrap_or_default();
        if meta.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "the container has no name",
            ));
        }
        let wanted = config
            .image
            .as_ref()
            .map(|i| i.image.clone())
            .unwrap_or_default();
        let image = super::service::find_image(&wanted)?;
        let name = mapping::container_name(&pod_config.metadata.clone().unwrap_or_default(), &meta);
        let ns = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|s| s.namespace_options.clone())
            .unwrap_or_default();
        let mut found = Found::default();
        if ns.pid == pb::NamespaceMode::Target as i32 {
            let target = self.container_id(&ns.target_id)?;
            if let Ok(st) = state::reconcile(&self.dirs, &target) {
                if let (RunStatus::Running, Some(pid)) = (st.status, st.pid) {
                    found.target_pid_ns = Some(format!("/proc/{pid}/ns/pid"));
                }
            }
        }
        let linux = pod_config.linux.clone().unwrap_or_default();
        let view = PodView {
            hostname: std::fs::read_to_string(
                self.records.sandbox_dir(&sandbox_id).join("hostname"),
            )
            .map(|h| h.trim().to_string())
            .unwrap_or_default(),
            log_directory: pod_config.log_directory.clone(),
            cgroup_parent: linux.cgroup_parent.clone(),
            net: sandbox.net.clone(),
            ipc: sandbox.ipc.clone(),
            uts: sandbox.uts.clone(),
            pid: sandbox.pid.clone(),
            files: self.records.sandbox_dir(&sandbox_id).display().to_string(),
            shm: sandbox.shm.clone(),
            labels: Default::default(),
        };
        let id = crate::id::new_id().map_err(err)?;
        let driver = self.options.cgroup_driver;
        let made = crate::container::create_on_image(&self.dirs, &image, &id, |rootfs, digest| {
            mapping::container_spec(
                &id, &name, &config, &view, &image, rootfs, digest, driver, &found,
            )
        });
        if let Err(e) = made {
            let _ = state::remove_container_dir(&self.dirs, &id);
            return Err(match e.kind {
                ErrorKind::Conflict => Status::new(Code::AlreadyExists, e.to_string()),
                _ => err(e),
            });
        }
        let record = records::Container {
            id: id.clone(),
            sandbox: sandbox_id,
            created: now_ns(),
            image: wanted,
            image_ref: image.digest.clone(),
            log_path: mapping::log_file(&pod_config.log_directory, &config.log_path),
            resources: config.linux.as_ref().and_then(|l| l.resources.clone()),
            start_error: None,
        };
        if let Err(e) = self.records.save_container(&record, Some(&config)) {
            let _ = ContainerHandle::from_id(id.clone()).remove(true);
            return Err(err(e));
        }
        self.events
            .send(&id, pb::ContainerEventType::ContainerCreatedEvent);
        Ok(id)
    }

    pub fn start_container(&self, given: &str) -> Answer<()> {
        let id = self.container_id(given)?;
        let lock = self.lock(&id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut record, _) = self.records.container(&id).map_err(err)?;
        let (sandbox, _) = self.records.sandbox(&record.sandbox).map_err(err)?;
        if !sandbox.ready {
            return Err(Status::new(
                Code::FailedPrecondition,
                format!("pod sandbox {} is not ready", record.sandbox),
            ));
        }
        let st = state::reconcile(&self.dirs, &id).map_err(err)?;
        if st.status != RunStatus::Created || record.start_error.is_some() {
            return Err(Status::new(
                Code::FailedPrecondition,
                format!("container {id} is not in the created state"),
            ));
        }
        if let Err(e) = ContainerHandle::from_id(id.clone()).start() {
            record.start_error = Some((now_ns(), e.to_string()));
            let _ = self.records.save_container(&record, None);
            self.events
                .send(&id, pb::ContainerEventType::ContainerStoppedEvent);
            return Err(err(e));
        }
        self.events
            .send(&id, pb::ContainerEventType::ContainerStartedEvent);
        Ok(())
    }

    pub fn stop_container(&self, given: &str, timeout: i64) -> Answer<()> {
        let id = self.container_id(given)?;
        self.stop_container_now(&id, Duration::from_secs(timeout.max(0) as u64))
    }

    /// The stop signal, then SIGKILL after `timeout` when it still runs.
    fn stop_container_now(&self, id: &str, timeout: Duration) -> Answer<()> {
        let st = match state::reconcile(&self.dirs, id) {
            Ok(st) => st,
            Err(e) if e.kind == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(err(e)),
        };
        if st.status != RunStatus::Running {
            return Ok(());
        }
        let handle = ContainerHandle::from_id(id.to_string());
        if !timeout.is_zero() {
            let signal = self
                .records
                .container(id)
                .map(|(_, c)| stop_signal(c.stop_signal))
                .unwrap_or(libc::SIGTERM);
            if handle.kill(signal).is_ok() {
                if let Ok(Some(_)) = handle.wait_up_to(timeout) {
                    self.events
                        .send(id, pb::ContainerEventType::ContainerStoppedEvent);
                    return Ok(());
                }
            }
        }
        let _ = handle.kill(libc::SIGKILL);
        match handle.wait_up_to(Duration::from_secs(30)) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(Status::new(
                    Code::DeadlineExceeded,
                    format!("container {id} did not end after SIGKILL"),
                ));
            }
            Err(e) if e.kind == ErrorKind::NotFound => {}
            Err(_) => {}
        }
        self.events
            .send(id, pb::ContainerEventType::ContainerStoppedEvent);
        Ok(())
    }

    pub fn remove_container(&self, given: &str) -> Answer<()> {
        // Removing what is gone already succeeds.
        let Ok(id) = self.container_id(given) else {
            return Ok(());
        };
        let lock = self.lock(&id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_container_now(&id, Duration::ZERO)?;
        match ContainerHandle::from_id(id.clone()).remove(true) {
            Ok(()) => {}
            Err(e) if e.kind == ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
        self.records.remove_container(&id).map_err(err)?;
        self.events
            .send(&id, pb::ContainerEventType::ContainerDeletedEvent);
        Ok(())
    }

    pub fn container_status(
        &self,
        given: &str,
        verbose: bool,
    ) -> Answer<pb::ContainerStatusResponse> {
        let id = self.container_id(given)?;
        let status = self.status_of(&id)?;
        let mut info = HashMap::new();
        if verbose {
            let st = state::read_state(&self.dirs, &id).ok();
            let spec = state::read_config(&self.dirs, &id).ok();
            info.insert(
                "info".to_string(),
                serde_json::json!({
                    "pid": st.as_ref().and_then(|s| s.pid),
                    "cgroup": st.as_ref().and_then(|s| s.cgroup.clone()),
                    "sandboxID": self.records.container(&id).ok().map(|(r, _)| r.sandbox),
                    "config": spec,
                })
                .to_string(),
            );
        }
        Ok(pb::ContainerStatusResponse {
            status: Some(status),
            info,
        })
    }

    pub(crate) fn status_of(&self, id: &str) -> Answer<pb::ContainerStatus> {
        let (record, config) = self.records.container(id).map_err(err)?;
        let st = state::reconcile(&self.dirs, id).ok();
        let secs = |s: Option<u64>| s.map_or(0, |s| s as i64 * 1_000_000_000);
        let (state, exit, reason, message, started, finished) = match (&st, &record.start_error) {
            (Some(s), Some((at, why))) if s.status == RunStatus::Created => (
                pb::ContainerState::ContainerExited,
                128,
                "StartError".to_string(),
                why.clone(),
                0,
                *at,
            ),
            (Some(s), _) => {
                let state = match s.status {
                    RunStatus::Created => pb::ContainerState::ContainerCreated,
                    RunStatus::Running => pb::ContainerState::ContainerRunning,
                    RunStatus::Exited => pb::ContainerState::ContainerExited,
                };
                let code = s.exit_code.unwrap_or(0);
                let reason = match (s.status, s.oom, code) {
                    (RunStatus::Exited, Some(_), _) => "OOMKilled",
                    (RunStatus::Exited, None, 0) => "Completed",
                    (RunStatus::Exited, None, _) => "Error",
                    _ => "",
                };
                let finished = match s.status {
                    RunStatus::Exited => secs(s.finished).max(secs(s.started)),
                    _ => 0,
                };
                (
                    state,
                    code,
                    reason.to_string(),
                    String::new(),
                    secs(s.started),
                    finished,
                )
            }
            (None, _) => (
                pb::ContainerState::ContainerUnknown,
                0,
                String::new(),
                String::new(),
                0,
                0,
            ),
        };
        Ok(pb::ContainerStatus {
            id: id.to_string(),
            metadata: config.metadata.clone(),
            state: state as i32,
            created_at: record.created,
            started_at: started,
            finished_at: finished,
            exit_code: exit,
            image: Some(pb::ImageSpec {
                image: record.image.clone(),
                ..Default::default()
            }),
            image_ref: record.image_ref.clone(),
            reason,
            message,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            mounts: config.mounts.clone(),
            log_path: record.log_path.clone().unwrap_or_default(),
            resources: Some(pb::ContainerResources {
                linux: record.resources.clone(),
                windows: None,
            }),
            image_id: record.image_ref.clone(),
            user: None,
            stop_signal: config.stop_signal,
        })
    }

    pub fn list_containers(
        &self,
        filter: Option<pb::ContainerFilter>,
    ) -> Answer<Vec<pb::Container>> {
        let filter = filter.unwrap_or_default();
        let mut out = Vec::new();
        for id in self.records.container_ids() {
            if !id.starts_with(&filter.id) {
                continue;
            }
            let Ok((record, config)) = self.records.container(&id) else {
                continue;
            };
            if !record.sandbox.starts_with(&filter.pod_sandbox_id) {
                continue;
            }
            if !mapping::matches_labels(&config.labels, &filter.label_selector) {
                continue;
            }
            let Ok(status) = self.status_of(&id) else {
                continue;
            };
            if filter
                .state
                .as_ref()
                .is_some_and(|f| f.state != status.state)
            {
                continue;
            }
            out.push(pb::Container {
                id,
                pod_sandbox_id: record.sandbox,
                metadata: config.metadata,
                image: status.image,
                image_ref: status.image_ref,
                state: status.state,
                created_at: status.created_at,
                labels: config.labels,
                annotations: config.annotations,
                image_id: status.image_id,
            });
        }
        Ok(out)
    }

    pub fn update_container_resources(
        &self,
        given: &str,
        resources: Option<pb::LinuxContainerResources>,
    ) -> Answer<()> {
        let id = self.container_id(given)?;
        let lock = self.lock(&id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(resources) = resources else {
            return Ok(());
        };
        let (mut record, _) = self.records.container(&id).map_err(err)?;
        let files = mapping::resource_files(&resources);
        let st = state::reconcile(&self.dirs, &id).map_err(err)?;
        if let (RunStatus::Running, Some(path)) = (st.status, &st.cgroup) {
            crate::cgroup::write_files(Path::new(path), &files).map_err(err)?;
        }
        // The next start (and the status) use them too.
        let _ = state::change_settings(&self.dirs, &id, |spec| {
            spec.limits.files.extend(files.clone());
            Ok(())
        });
        record.resources = Some(resources);
        self.records.save_container(&record, None).map_err(err)
    }

    pub fn reopen_container_log(&self, given: &str) -> Answer<()> {
        let id = self.container_id(given)?;
        let st = state::reconcile(&self.dirs, &id).map_err(err)?;
        if st.status != RunStatus::Running {
            return Err(Status::new(
                Code::FailedPrecondition,
                format!("container {id} is not running"),
            ));
        }
        let (record, _) = self.records.container(&id).map_err(err)?;
        if let Some(path) = record.log_path {
            // The writer finds the new file at its next line.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| err(Error::io(format!("cannot open {path}"), &e, "")))?;
        }
        Ok(())
    }

    pub fn exec_sync(
        &self,
        given: &str,
        cmd: Vec<String>,
        timeout: i64,
    ) -> Answer<pb::ExecSyncResponse> {
        use std::io::Read;
        let id = self.container_id(given)?;
        if cmd.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "no command to run"));
        }
        let pipe = || {
            rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
                .map_err(|e| err(Error::io("cannot run the command", &e.into(), "")))
        };
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let running = ContainerHandle::from_id(id.clone())
            .exec_with(
                &cmd,
                &ExecOptions::default(),
                std::process::Stdio::null(),
                std::process::Stdio::from(out_w),
                std::process::Stdio::from(err_w),
                None,
            )
            .map_err(err)?;
        let read = |fd: std::os::fd::OwnedFd| {
            std::thread::spawn(move || {
                let mut file = std::fs::File::from(fd);
                let mut kept = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => return kept,
                        Ok(n) => {
                            let room = EXEC_OUTPUT_LIMIT.saturating_sub(kept.len());
                            kept.extend_from_slice(&buf[..n.min(room)]);
                        }
                    }
                }
            })
        };
        let (out, errs) = (read(out_r), read(err_r));
        let helper = running.helper_pid();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(running.wait());
        });
        let code = if timeout > 0 {
            match rx.recv_timeout(Duration::from_secs(timeout as u64)) {
                Ok(r) => r,
                Err(_) => {
                    // SAFETY: kill has no memory arguments; the helper's command dies with it.
                    unsafe { libc::kill(helper as libc::pid_t, libc::SIGKILL) };
                    let _ = rx.recv();
                    let _ = (out.join(), errs.join());
                    return Err(Status::new(
                        Code::DeadlineExceeded,
                        format!("command {cmd:?} timed out after {timeout}s"),
                    ));
                }
            }
        } else {
            rx.recv().unwrap_or_else(|_| Ok(-1))
        };
        let code = code.map_err(err)?;
        Ok(pb::ExecSyncResponse {
            stdout: out.join().unwrap_or_default(),
            stderr: errs.join().unwrap_or_default(),
            exit_code: code,
        })
    }

    // ----- Stats -----

    pub fn container_stats(&self, given: &str) -> Answer<pb::ContainerStats> {
        let id = self.container_id(given)?;
        let (_, config) = self.records.container(&id).map_err(err)?;
        let st = state::reconcile(&self.dirs, &id).ok();
        let cgroup = st.as_ref().and_then(|s| s.cgroup.clone());
        let now = now_ns();
        let usage = cgroup
            .as_deref()
            .map(super::stats::read)
            .unwrap_or_default();
        let upper = self.dirs.container_dir(&id).join("upper");
        let (bytes, inodes) = super::stats::disk_usage(&upper);
        Ok(pb::ContainerStats {
            attributes: Some(pb::ContainerAttributes {
                id: id.clone(),
                metadata: config.metadata,
                labels: config.labels,
                annotations: config.annotations,
            }),
            cpu: Some(usage.cpu(now)),
            memory: Some(usage.memory(now)),
            writable_layer: Some(pb::FilesystemUsage {
                timestamp: now,
                fs_id: Some(pb::FilesystemIdentifier {
                    mountpoint: self.dirs.data.display().to_string(),
                }),
                used_bytes: Some(pb::UInt64Value { value: bytes }),
                inodes_used: Some(pb::UInt64Value { value: inodes }),
            }),
            swap: Some(usage.swap(now)),
            io: None,
        })
    }

    pub fn list_container_stats(
        &self,
        filter: Option<pb::ContainerStatsFilter>,
    ) -> Answer<Vec<pb::ContainerStats>> {
        let filter = filter.unwrap_or_default();
        let wanted = pb::ContainerFilter {
            id: filter.id,
            pod_sandbox_id: filter.pod_sandbox_id,
            label_selector: filter.label_selector,
            state: None,
        };
        let mut out = Vec::new();
        for c in self.list_containers(Some(wanted))? {
            if let Ok(s) = self.container_stats(&c.id) {
                out.push(s);
            }
        }
        Ok(out)
    }

    pub fn pod_sandbox_stats(&self, given: &str) -> Answer<pb::PodSandboxStats> {
        let id = self.sandbox_id(given)?;
        let (s, config) = self.records.sandbox(&id).map_err(err)?;
        let now = now_ns();
        let parent = config
            .linux
            .as_ref()
            .map(|l| l.cgroup_parent.clone())
            .unwrap_or_default();
        let usage = mapping::cgroup_path(self.options.cgroup_driver, &parent, "x")
            .ok()
            .and_then(|p| Path::new(&p).parent().map(Path::to_path_buf))
            .filter(|_| !parent.is_empty())
            .map(|p| super::stats::read(&p.display().to_string()))
            .unwrap_or_default();
        let containers: Vec<pb::ContainerStats> = self
            .containers_of(&id)
            .iter()
            .filter_map(|c| self.container_stats(c).ok())
            .collect();
        let network = s.net.as_deref().map(|ns| super::stats::network(ns, now));
        Ok(pb::PodSandboxStats {
            attributes: Some(pb::PodSandboxAttributes {
                id: id.clone(),
                metadata: config.metadata,
                labels: config.labels,
                annotations: config.annotations,
            }),
            linux: Some(pb::LinuxPodSandboxStats {
                cpu: Some(usage.cpu(now)),
                memory: Some(usage.memory(now)),
                network,
                process: Some(pb::ProcessUsage {
                    timestamp: now,
                    process_count: usage.pids.map(|value| pb::UInt64Value { value }),
                }),
                containers,
                io: None,
            }),
            windows: None,
        })
    }

    pub fn list_pod_sandbox_stats(
        &self,
        filter: Option<pb::PodSandboxStatsFilter>,
    ) -> Answer<Vec<pb::PodSandboxStats>> {
        let filter = filter.unwrap_or_default();
        let wanted = pb::PodSandboxFilter {
            id: filter.id,
            label_selector: filter.label_selector,
            state: None,
        };
        let mut out = Vec::new();
        for s in self.list_pod_sandbox(Some(wanted))? {
            if let Ok(stats) = self.pod_sandbox_stats(&s.id) {
                out.push(stats);
            }
        }
        Ok(out)
    }

    /// The main process of a running container (for streaming).
    pub fn running_pid(&self, given: &str) -> Answer<(String, u32)> {
        let id = self.container_id(given)?;
        let st = state::reconcile(&self.dirs, &id).map_err(err)?;
        match (st.status, st.pid) {
            (RunStatus::Running, Some(pid)) => Ok((id, pid)),
            _ => Err(Status::new(
                Code::FailedPrecondition,
                format!("container {id} is not running"),
            )),
        }
    }

    /// The network namespace of a ready pod (for port forwarding); None: the host's.
    pub fn sandbox_netns(&self, given: &str) -> Answer<(String, Option<String>)> {
        let id = self.sandbox_id(given)?;
        let (s, _) = self.records.sandbox(&id).map_err(err)?;
        if !s.ready {
            return Err(Status::new(
                Code::FailedPrecondition,
                format!("pod sandbox {id} is not ready"),
            ));
        }
        Ok((id, s.net))
    }

    /// Where a container's log is.
    pub fn log_path(&self, id: &str) -> Option<String> {
        self.records
            .container(id)
            .ok()
            .and_then(|(r, _)| r.log_path)
    }

    /// The pod of a container.
    pub fn sandbox_of(&self, id: &str) -> Option<String> {
        self.records.container(id).ok().map(|(r, _)| r.sandbox)
    }

    /// A container's config, as the kubelet gave it.
    pub fn container_config(&self, given: &str) -> Answer<(String, pb::ContainerConfig)> {
        let id = self.container_id(given)?;
        let (_, config) = self.records.container(&id).map_err(err)?;
        Ok((id, config))
    }
}

fn sandbox_state(s: &records::Sandbox) -> i32 {
    if s.ready {
        pb::PodSandboxState::SandboxReady as i32
    } else {
        pb::PodSandboxState::SandboxNotready as i32
    }
}

/// The signal a CRI stop signal names; SIGTERM by default.
pub fn stop_signal(signal: i32) -> i32 {
    let name = pb::Signal::try_from(signal)
        .map(|s| s.as_str_name())
        .unwrap_or("RUNTIME_DEFAULT");
    match name {
        "SIGINT" => libc::SIGINT,
        "SIGQUIT" => libc::SIGQUIT,
        "SIGKILL" => libc::SIGKILL,
        "SIGHUP" => libc::SIGHUP,
        "SIGUSR1" => libc::SIGUSR1,
        "SIGUSR2" => libc::SIGUSR2,
        "SIGWINCH" => libc::SIGWINCH,
        "SIGSTOP" => libc::SIGSTOP,
        "SIGPWR" => libc::SIGPWR,
        _ => libc::SIGTERM,
    }
}
