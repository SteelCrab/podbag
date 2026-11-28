#![cfg(target_os = "linux")]
//! An opt-in test with two isolated Podbag engines and a private OpenSSH server.

mod common;
use common::api::{Service, docker_out, wait_for};
use common::registry::{FakeRegistry, TestImage, alpine_layer};
use common::{BIN, Sandbox, network_turn, root_mode, stderr, stdout};
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    stdout(&output)
}

struct SshWorker {
    daemon: Child,
    files: tempfile::TempDir,
    config: PathBuf,
}

impl SshWorker {
    fn start(worker: &Service) -> Self {
        let files = tempfile::tempdir().unwrap();
        let path = files.path();
        for key in ["host", "client"] {
            checked(
                Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(path.join(key))
                    .output()
                    .unwrap(),
            );
        }
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let known = format!(
            "[127.0.0.1]:{port} {}",
            fs::read_to_string(path.join("host.pub")).unwrap()
        );
        fs::write(path.join("known_hosts"), known).unwrap();
        fs::copy(path.join("client.pub"), path.join("authorized_keys")).unwrap();
        let allowed = path.join("allowed");
        fs::write(&allowed, "").unwrap();
        let command = format!(
            "test -e {} || exit 1\nexec {} system dial-stdio --socket {}\n",
            shlex::try_quote(allowed.to_str().unwrap()).unwrap(),
            shlex::try_quote(BIN).unwrap(),
            shlex::try_quote(worker.socket.to_str().unwrap()).unwrap(),
        );
        fs::write(path.join("connect"), command).unwrap();
        let server = format!(
            "ListenAddress 127.0.0.1\nPort {port}\nHostKey {dir}/host\nPidFile {dir}/pid\nAuthorizedKeysFile {dir}/authorized_keys\nStrictModes no\nLogLevel ERROR\nPermitRootLogin prohibit-password\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nForceCommand /bin/sh {dir}/connect\n",
            dir = path.display(),
        );
        fs::write(path.join("sshd.conf"), server).unwrap();
        let config = path.join("ssh.conf");
        let client = format!(
            "Host rollout-worker\n  HostName 127.0.0.1\n  User root\n  Port {port}\n  IdentityFile {dir}/client\n  UserKnownHostsFile {dir}/known_hosts\n  IdentitiesOnly yes\n",
            dir = path.display(),
        );
        fs::write(&config, client).unwrap();
        let daemon = Command::new("sudo")
            .args(["-n", "/usr/sbin/sshd", "-D", "-e", "-f"])
            .arg(path.join("sshd.conf"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let ssh = Self {
            daemon,
            files,
            config,
        };
        assert!(wait_for(Duration::from_secs(5), || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        }));
        ssh
    }

    fn set_reachable(&self, reachable: bool) {
        let marker = self.files.path().join("allowed");
        if reachable {
            fs::write(marker, "").unwrap();
        } else {
            fs::remove_file(marker).unwrap();
        }
    }
}

impl Drop for SshWorker {
    fn drop(&mut self) {
        // SAFETY: this is our own unreaped sudo process, which forwards the signal.
        unsafe { libc::kill(self.daemon.id() as i32, libc::SIGTERM) };
        let _ = self.daemon.wait();
    }
}

fn running(manager: &Sandbox, service: &Service) -> Vec<(String, String)> {
    checked(docker_out(
        manager,
        service,
        &[
            "service",
            "ps",
            "pinned",
            "--format",
            "{{.Node}}|{{.Image}}|{{.CurrentState}}",
        ],
    ))
    .lines()
    .filter_map(|line| {
        let mut parts = line.split('|');
        let node = parts.next()?;
        let image = parts.next()?;
        parts
            .next()?
            .starts_with("Running")
            .then(|| (node.to_owned(), image.to_owned()))
    })
    .collect()
}

fn status(manager: &Sandbox, service: &Service) -> String {
    checked(docker_out(
        manager,
        service,
        &[
            "service",
            "inspect",
            "pinned",
            "--format",
            "{{.UpdateStatus.State}}",
        ],
    ))
}

#[test]
fn pinned_workers_roll_and_roll_back_over_real_ssh() {
    if std::env::var("PODBAG_TEST_SWARM_SSH").as_deref() != Ok("1") || !root_mode() {
        eprintln!("set PODBAG_TEST_SWARM_SSH=1 and PODBAG_TEST_ROOT=1 to run the OpenSSH fixture");
        return;
    }
    let _turn = network_turn();
    let registry = FakeRegistry::start(false);
    for tag in ["one", "two"] {
        registry.push(
            "app",
            tag,
            &TestImage {
                layers: vec![alpine_layer()],
                config: serde_json::json!({"Cmd": ["sh", "-c", "trap 'exit 0' TERM; while true; do sleep 1; done"]}),
            },
        );
    }
    let one = format!("{}/app:one", registry.host);
    let two = format!("{}/app:two", registry.host);
    let worker = Sandbox::new();
    let worker_service = Service::start(&worker, &["--timeout", "0"]);
    let ssh = SshWorker::start(&worker_service);
    let manager = Sandbox::new();
    let mut service = Service::start(&manager, &["--timeout", "0"]);
    checked(docker_out(&manager, &service, &["swarm", "init"]));
    let worker_id = checked(
        manager
            .podbag()
            .args([
                "swarm",
                "worker-add",
                "rollout-worker",
                "rollout-worker",
                "--ssh-config",
                ssh.config.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    let constraint = format!("node.id=={worker_id}");
    checked(docker_out(
        &manager,
        &service,
        &[
            "service",
            "create",
            "--detach=true",
            "--name",
            "pinned",
            "--replicas",
            "2",
            "--constraint",
            &constraint,
            "--update-parallelism",
            "1",
            "--update-delay",
            "3s",
            "--update-monitor",
            "1s",
            "--rollback-monitor",
            "1s",
            "--stop-grace-period",
            "1s",
            &one,
        ],
    ));
    assert!(wait_for(Duration::from_secs(15), || running(
        &manager, &service
    )
    .len()
        == 2));
    let rejected = docker_out(
        &manager,
        &service,
        &[
            "service",
            "update",
            "--detach=true",
            "--image",
            &two,
            "--update-order",
            "start-first",
            "pinned",
        ],
    );
    assert!(!rejected.status.success());
    let message = stderr(&rejected);
    assert!(message.contains("stop-first"), "{message}");
    let old = running(&manager, &service);
    assert_eq!(old.len(), 2);
    assert!(old.iter().all(|(_, image)| image.starts_with(&one)));
    checked(docker_out(
        &manager,
        &service,
        &[
            "service",
            "update",
            "--detach=true",
            "--image",
            &two,
            "pinned",
        ],
    ));
    assert!(wait_for(Duration::from_secs(15), || {
        let tasks = running(&manager, &service);
        tasks.len() == 2
            && tasks.iter().all(|(node, _)| node == "rollout-worker")
            && tasks
                .iter()
                .filter(|(_, image)| image.starts_with(&two))
                .count()
                == 1
    }));
    println!(
        "PASS: stop-first rollout replaced one pinned replica at a time on its original SSH worker"
    );

    ssh.set_reachable(false);
    drop(service);
    service = Service::start(&manager, &["--timeout", "0"]);
    assert_eq!(status(&manager, &service), "updating");
    assert!(checked(manager.podbag().args(["ps", "-aq"]).output().unwrap()).is_empty());
    ssh.set_reachable(true);
    assert!(wait_for(Duration::from_secs(25), || status(
        &manager, &service
    ) == "completed"));
    let tasks = running(&manager, &service);
    assert_eq!(tasks.len(), 2);
    assert!(
        tasks
            .iter()
            .all(|(node, image)| node == "rollout-worker" && image.starts_with(&two))
    );
    println!(
        "PASS: manager restart and SSH disconnection kept the rollout revision and replica placement"
    );

    checked(docker_out(
        &manager,
        &service,
        &["service", "rollback", "--detach=true", "pinned"],
    ));
    assert!(wait_for(Duration::from_secs(20), || status(
        &manager, &service
    ) == "rollback_completed"));
    assert!(
        running(&manager, &service)
            .iter()
            .all(|(_, image)| image.starts_with(&one))
    );
    checked(docker_out(
        &manager,
        &service,
        &[
            "service",
            "update",
            "--detach=true",
            "--entrypoint",
            "/bin/false",
            "--restart-condition",
            "none",
            "pinned",
        ],
    ));
    assert!(wait_for(Duration::from_secs(20), || status(
        &manager, &service
    ) == "paused"));
    assert_eq!(running(&manager, &service).len(), 1);
    checked(docker_out(
        &manager,
        &service,
        &["service", "rollback", "--detach=true", "pinned"],
    ));
    assert!(wait_for(Duration::from_secs(20), || status(
        &manager, &service
    ) == "rollback_completed"));
    let tasks = running(&manager, &service);
    assert_eq!(tasks.len(), 2);
    assert!(
        tasks
            .iter()
            .all(|(node, image)| node == "rollout-worker" && image.starts_with(&one))
    );
    println!(
        "PASS: manual rollback and rollback after a failed replica restored both pinned slots"
    );

    checked(docker_out(&manager, &service, &["service", "rm", "pinned"]));
    assert!(checked(worker.podbag().args(["ps", "-aq"]).output().unwrap()).is_empty());
    checked(docker_out(&manager, &service, &["node", "rm", &worker_id]));
    checked(docker_out(
        &manager,
        &service,
        &["swarm", "leave", "--force"],
    ));
    println!("PASS: service removal and worker unenrollment left no tasks");
}
