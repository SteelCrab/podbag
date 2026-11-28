//! An opt-in Ollama process owned by Desktop and bound to an isolated loopback port.
//!
//! Only [`Action::Start`] starts a backend, and only [`Action::Pull`] downloads. The
//! models are kept in the app's data folder; the process stops with the app.

use podbag::client::choice::find_tool;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::lock;

const PROBE: Duration = Duration::from_secs(2);
const REQUEST: Duration = Duration::from_secs(12);
const DOWNLOAD: Duration = Duration::from_secs(30 * 60);
const INFERENCE: Duration = Duration::from_secs(5 * 60);
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;
/// The longest prompt, in characters.
pub const MAX_PROMPT: usize = 16_000;

/// Why an action failed. `kind` names the text the UI shows (`models.error.<kind>`):
/// `missing`, `start`, `stopped`, `modelName`, `notInstalled`, `prompt`, `backend` or
/// `busy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Failure {
    pub fn new(kind: &'static str) -> Self {
        Self { kind, detail: None }
    }

    pub fn with_detail(kind: &'static str, detail: impl ToString) -> Self {
        Self {
            kind,
            detail: Some(detail.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, Failure>;

/// What the window asks the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Status,
    Start,
    Stop,
    Pull { model: String },
    Run { model: String, prompt: String },
    Unload { model: String },
}

impl Action {
    /// `status`, `start`, `stop`, `pull`, `run` or `unload`.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Status => "status",
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Pull { .. } => "pull",
            Action::Run { .. } => "run",
            Action::Unload { .. } => "unload",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Status(Status),
    Generation(Generation),
}

/// The runner: whether it runs, its version, and the models it has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub running: bool,
    pub version: String,
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub name: String,
    pub size: u64,
    /// In memory now.
    pub loaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Generation {
    pub response: String,
    #[serde(skip_serializing)]
    pub done: bool,
}

#[derive(Deserialize)]
struct Version {
    version: String,
}

#[derive(Deserialize)]
struct ModelList {
    models: Option<Vec<Artifact>>,
}

#[derive(Deserialize)]
struct Artifact {
    name: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct PullResult {
    status: String,
}

struct Backend {
    address: SocketAddrV4,
}

impl Backend {
    fn request<T: DeserializeOwned>(
        &self,
        path: &str,
        payload: Option<Value>,
        timeout: Duration,
    ) -> Result<T> {
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(PROBE))
            .timeout_global(Some(timeout))
            .build()
            .new_agent();
        let url = format!("http://{}{path}", self.address);
        let response = match payload {
            Some(body) => agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string().as_bytes()),
            None => agent.get(&url).call(),
        }
        .map_err(|error| Failure::with_detail("backend", error))?;
        let (parts, mut body) = response.into_parts();
        let bytes = body
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_vec()
            .map_err(|error| Failure::with_detail("backend", error))?;
        if !parts.status.is_success() {
            if let Ok(error) = serde_json::from_slice::<Value>(&bytes)
                && let Some(message) = error["error"].as_str()
            {
                return Err(Failure::with_detail("backend", message));
            }
            return Err(Failure::with_detail("backend", parts.status));
        }
        serde_json::from_slice(&bytes).map_err(|error| Failure::with_detail("backend", error))
    }

    fn models(&self) -> Result<Vec<Artifact>> {
        let list: ModelList = self.request("/api/tags", None, REQUEST)?;
        Ok(list.models.unwrap_or_default())
    }

    fn status(&self) -> Result<Status> {
        let version: Version = self.request("/api/version", None, REQUEST)?;
        let running: ModelList = self.request("/api/ps", None, REQUEST)?;
        let running = running.models.unwrap_or_default();
        let models = self
            .models()?
            .into_iter()
            .map(|artifact| Model {
                loaded: running.iter().any(|model| model.name == artifact.name),
                name: artifact.name,
                size: artifact.size,
            })
            .collect();
        Ok(Status {
            version: version.version,
            running: true,
            models,
        })
    }

    fn pull(&self, name: &str) -> Result<()> {
        let name = registry_model(name)?;
        let report: PullResult = self.request(
            "/api/pull",
            Some(json!({"model": name, "stream": false})),
            DOWNLOAD,
        )?;
        if report.status == "success" {
            Ok(())
        } else {
            Err(Failure::with_detail("backend", report.status))
        }
    }

    fn installed(&self, name: &str) -> Result<()> {
        if !self.models()?.iter().any(|model| model.name == name) {
            return Err(Failure::new("notInstalled"));
        }
        Ok(())
    }

    fn run(&self, name: &str, prompt: &str) -> Result<Generation> {
        check_prompt(prompt)?;
        self.installed(name)?;
        self.generate(json!({
            "model": name,
            "prompt": prompt,
            "stream": false,
            "options": {"num_predict": 512},
        }))
    }

    fn unload(&self, name: &str) -> Result<()> {
        self.installed(name)?;
        self.generate(json!({"model": name, "keep_alive": 0, "stream": false}))?;
        Ok(())
    }

    fn generate(&self, request: Value) -> Result<Generation> {
        let output: Generation = self.request("/api/generate", Some(request), INFERENCE)?;
        if !output.done {
            return Err(Failure::new("backend"));
        }
        Ok(output)
    }
}

/// A prompt that is not blank and not longer than [`MAX_PROMPT`].
pub fn check_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() || prompt.chars().count() > MAX_PROMPT {
        return Err(Failure::new("prompt"));
    }
    Ok(())
}

/// An Ollama registry model, with no external host, local path, or cloud tag.
pub fn registry_model(input: &str) -> Result<String> {
    let input = input.trim();
    let (path, tag) = input.split_once(':').unwrap_or((input, "latest"));
    let (namespace, name) = path.split_once('/').unwrap_or(("library", path));
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 128
            && (part.as_bytes()[0].is_ascii_lowercase() || part.as_bytes()[0].is_ascii_digit())
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
            })
    };
    if !valid(namespace)
        || namespace.contains('.')
        || !valid(name)
        || !valid(tag)
        || tag == "cloud"
        || tag.ends_with("-cloud")
    {
        return Err(Failure::new("modelName"));
    }
    Ok(format!("registry.ollama.ai/{namespace}/{name}:{tag}"))
}

struct Runtime {
    backend: Backend,
    child: Mutex<Child>,
    operation: Mutex<()>,
    _work: tempfile::TempDir,
}

impl Runtime {
    fn start(models: &Path) -> Result<Self> {
        let scanner = find_tool("ollama").ok_or_else(|| Failure::new("missing"))?;
        let program = scanner
            .canonicalize()
            .map_err(|error| Failure::with_detail("missing", error))?;
        std::fs::create_dir_all(models).map_err(|error| Failure::with_detail("start", error))?;
        let models = models
            .canonicalize()
            .map_err(|error| Failure::with_detail("start", error))?;
        let work = tempfile::tempdir().map_err(|error| Failure::with_detail("start", error))?;
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map_err(|error| Failure::with_detail("start", error))?
            .port();
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let mut command = Command::new(program);
        command
            .arg("serve")
            .current_dir(work.path())
            .env_clear()
            .env("HOME", work.path())
            .env("USERPROFILE", work.path())
            .env("XDG_CONFIG_HOME", work.path())
            .env("TMPDIR", work.path())
            .env("TEMP", work.path())
            .env("OLLAMA_HOST", address.to_string())
            .env("OLLAMA_MODELS", models)
            .env("OLLAMA_NO_CLOUD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            if let Some(root) = std::env::var_os("SystemRoot") {
                command.env("SystemRoot", root);
            }
            command.creation_flags(0x0800_0000);
        }
        let child = command
            .spawn()
            .map_err(|error| Failure::with_detail("start", error))?;
        let runtime = Self {
            backend: Backend { address },
            child: Mutex::new(child),
            operation: Mutex::new(()),
            _work: work,
        };
        let deadline = Instant::now() + Duration::from_secs(12);
        while runtime.live() && Instant::now() < deadline {
            if runtime
                .backend
                .request::<Version>("/api/version", None, PROBE)
                .is_ok()
                && runtime.live()
            {
                return Ok(runtime);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(Failure::new("start"))
    }

    fn live(&self) -> bool {
        matches!(lock(&self.child).try_wait(), Ok(None))
    }

    fn stop(&self) {
        let mut child = lock(&self.child);
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        #[cfg(unix)]
        {
            // The isolated process group contains this server and the runners it launched.
            // SAFETY: signal only the group made for this still-running child.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            // SAFETY: the child still owns the process group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            use std::os::windows::process::CommandExt;
            let _ = Command::new(std::path::PathBuf::from(root).join("System32/taskkill.exe"))
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .creation_flags(0x0800_0000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The runner of the app: none until Start.
#[derive(Default)]
pub struct ModelRunner(Mutex<Option<Arc<Runtime>>>);

impl ModelRunner {
    /// Stop the backend, if one runs (Stop, and when the app ends).
    pub fn stop(&self) {
        if let Some(runtime) = lock(&self.0).take() {
            runtime.stop();
        }
    }

    /// Do `action`; the models are kept in `store`. Slow: a start waits for the
    /// backend, a pull downloads and a run generates.
    pub fn action(&self, action: Action, store: &Path) -> Result<Reply> {
        let runtime = match action {
            Action::Start => {
                let mut current = lock(&self.0);
                if current.as_ref().is_none_or(|runtime| !runtime.live()) {
                    *current = Some(Arc::new(Runtime::start(store)?));
                }
                current.clone()
            }
            Action::Stop => {
                self.stop();
                None
            }
            _ => {
                let mut current = lock(&self.0);
                if current.as_ref().is_some_and(|runtime| !runtime.live()) {
                    *current = None;
                }
                current.clone()
            }
        };
        let Some(runtime) = runtime else {
            return if matches!(action, Action::Status | Action::Stop) {
                Ok(Reply::Status(Status::default()))
            } else {
                Err(Failure::new("stopped"))
            };
        };
        let result = match action {
            Action::Start | Action::Status => runtime.backend.status().map(Reply::Status),
            Action::Stop => unreachable!("the runtime was removed above"),
            action => {
                let _operation = runtime
                    .operation
                    .try_lock()
                    .map_err(|_| Failure::new("busy"))?;
                match action {
                    Action::Pull { model } => runtime
                        .backend
                        .pull(&model)
                        .and_then(|()| runtime.backend.status())
                        .map(Reply::Status),
                    Action::Run { model, prompt } => {
                        runtime.backend.run(&model, &prompt).map(Reply::Generation)
                    }
                    Action::Unload { model } => runtime
                        .backend
                        .unload(&model)
                        .and_then(|()| runtime.backend.status())
                        .map(Reply::Status),
                    _ => unreachable!("handled above"),
                }
            }
        };
        if !lock(&self.0)
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &runtime))
        {
            return Err(Failure::new("stopped"));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    fn fake(
        responses: Vec<(&'static str, Value)>,
    ) -> (Backend, std::thread::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let std::net::SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        let requests = std::thread::spawn(move || {
            let mut payloads = Vec::new();
            for (path, response) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                let mut input = BufReader::new(&mut socket);
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert_eq!(line.split_whitespace().nth(1), Some(path));
                let mut length = 0;
                loop {
                    line.clear();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(size) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = size.trim().parse().unwrap();
                    }
                }
                if length > 0 {
                    let mut body = vec![0; length];
                    input.read_exact(&mut body).unwrap();
                    payloads.push(serde_json::from_slice(&body).unwrap());
                }
                let body = response.to_string();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            payloads
        });
        (Backend { address }, requests)
    }

    #[test]
    fn accepts_only_ollama_registry_model_names() {
        assert_eq!(
            registry_model("smollm2:135m").unwrap(),
            "registry.ollama.ai/library/smollm2:135m"
        );
        assert_eq!(
            registry_model("user/example").unwrap(),
            "registry.ollama.ai/user/example:latest"
        );
        for invalid in [
            "",
            "https://somewhere.test/m",
            "somewhere.test/m",
            "user/m/a",
            "../m",
            "m:cloud",
            "m:20b-cloud",
        ] {
            assert_eq!(registry_model(invalid).unwrap_err().kind, "modelName");
        }
    }

    #[test]
    fn backend_lists_downloads_runs_and_unloads_the_same_local_model() {
        let listing = json!({"models":[{"name":"smollm2:135m","size":123}]});
        let (backend, requests) = fake(vec![
            ("/api/pull", json!({"status":"success"})),
            ("/api/tags", listing.clone()),
            ("/api/generate", json!({"done":true,"response":"Blue."})),
            ("/api/version", json!({"version":"0.40.18"})),
            ("/api/ps", listing.clone()),
            ("/api/tags", listing.clone()),
            ("/api/tags", listing),
            ("/api/generate", json!({"done":true,"response":""})),
        ]);
        backend.pull("smollm2:135m").unwrap();
        assert_eq!(
            backend
                .run("smollm2:135m", "Name a color.")
                .unwrap()
                .response,
            "Blue."
        );
        let status = backend.status().unwrap();
        assert!(status.running);
        assert!(status.models[0].loaded);
        backend.unload("smollm2:135m").unwrap();
        let requests = requests.join().unwrap();
        assert_eq!(
            requests[0]["model"],
            "registry.ollama.ai/library/smollm2:135m"
        );
        assert_eq!(requests[1]["model"], "smollm2:135m");
        assert_eq!(requests[1]["prompt"], "Name a color.");
        assert_eq!(requests[2]["keep_alive"], 0);
    }

    #[test]
    fn inference_does_not_download_missing_models() {
        let (backend, requests) = fake(vec![("/api/tags", json!({"models":[]}))]);
        assert_eq!(
            backend.run("missing:latest", "Hello").unwrap_err().kind,
            "notInstalled"
        );
        assert!(requests.join().unwrap().is_empty());
    }

    #[test]
    fn prompts_must_have_text_and_a_limit() {
        assert!(check_prompt("Name a color.").is_ok());
        assert_eq!(check_prompt("  \n").unwrap_err().kind, "prompt");
        assert_eq!(
            check_prompt(&"a".repeat(MAX_PROMPT + 1)).unwrap_err().kind,
            "prompt"
        );
    }

    #[test]
    fn the_runner_starts_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let runner = ModelRunner::default();
        let Reply::Status(status) = runner.action(Action::Status, temp.path()).unwrap() else {
            unreachable!()
        };
        assert!(!status.running);
        assert_eq!(
            runner
                .action(
                    Action::Pull {
                        model: "smollm2:135m".into()
                    },
                    temp.path()
                )
                .err()
                .unwrap()
                .kind,
            "stopped"
        );
    }

    #[test]
    #[ignore = "requires local Ollama and downloads the small smollm2:135m artifact"]
    fn managed_ollama_smoke() {
        let temp = tempfile::tempdir().unwrap();
        let runner = ModelRunner::default();
        let run = |action| runner.action(action, temp.path()).unwrap();
        let Reply::Status(start) = run(Action::Start) else {
            unreachable!()
        };
        assert!(start.running && start.models.is_empty());
        let model = "smollm2:135m".to_string();
        let Reply::Status(pulled) = run(Action::Pull {
            model: model.clone(),
        }) else {
            unreachable!()
        };
        assert!(pulled.models.iter().any(|item| item.name == model));
        let Reply::Generation(output) = run(Action::Run {
            model: model.clone(),
            prompt: "Reply in one word with the color of a clear daytime sky.".into(),
        }) else {
            unreachable!()
        };
        assert!(!output.response.trim().is_empty());
        let Reply::Status(loaded) = run(Action::Status) else {
            unreachable!()
        };
        assert!(
            loaded
                .models
                .iter()
                .any(|item| item.name == model && item.loaded)
        );
        let Reply::Status(unloaded) = run(Action::Unload {
            model: model.clone(),
        }) else {
            unreachable!()
        };
        assert!(
            unloaded
                .models
                .iter()
                .any(|item| item.name == model && !item.loaded)
        );
        let Reply::Status(stopped) = run(Action::Stop) else {
            unreachable!()
        };
        assert!(!stopped.running);
    }
}
