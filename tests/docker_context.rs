#![cfg(unix)]
//! `podbag system docker status|use|restore` with a fake HOME (its docker config and
//! contexts) and a fake `docker` program on PATH: the real ~/.docker is never touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fake docker CLI: `context inspect|create|update|use|show`, on the files of
/// `$HOME/.docker` (as the real one keeps them), and a log of its calls.
const FAKE_DOCKER: &str = r#"#!/bin/sh
echo "$*" >> "$HOME/docker-calls"
cfg="$HOME/.docker"
sha() { printf %s "$1" | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1; }
meta() { echo "$cfg/contexts/meta/$(sha "$1")/meta.json"; }
current() { sed -n 's/.*"currentContext": *"\([^"]*\)".*/\1/p' "$cfg/config.json" 2>/dev/null; }
case "$1 $2" in
"context inspect")
  [ -f "$(meta "$3")" ] || { echo "context \"$3\": not found" >&2; exit 1; }
  printf '[%s]\n' "$(cat "$(meta "$3")")" ;;
"context create" | "context update")
  [ "$1 $2" = "context create" ] && [ -f "$(meta "$3")" ] && { echo "exists" >&2; exit 1; }
  mkdir -p "$(dirname "$(meta "$3")")"
  printf '{"Name":"%s","Endpoints":{"docker":{"Host":"%s"}}}' "$3" "${5#host=}" > "$(meta "$3")" ;;
"context use")
  [ "$3" = default ] || [ -f "$(meta "$3")" ] || { echo "context \"$3\": not found" >&2; exit 1; }
  printf '{"currentContext": "%s"}\n' "$3" > "$cfg/config.json"; echo "$3" ;;
"context show")
  c=$(current); echo "${c:-default}" ;;
*) echo "fake docker: $*" >&2; exit 1 ;;
esac
"#;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    /// A HOME whose docker CLI uses the context `desktop-linux` (Docker Desktop).
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = Home { dir };
        let desktop = home.path().join(".docker/run/docker.sock");
        home.docker(&[
            "context",
            "create",
            "desktop-linux",
            "--docker",
            &format!("host=unix://{}", desktop.display()),
        ]);
        home.docker(&["context", "use", "desktop-linux"]);
        let _ = std::fs::remove_file(home.path().join("docker-calls"));
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn env(&self, c: &mut Command) {
        c.env("HOME", self.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path().join("bin").display()),
            )
            .env("PODBAG_MAC_HOME", self.path().join("mac"))
            .env("PODBAG_UI_DIR", self.path().join("ui"))
            .env("XDG_RUNTIME_DIR", self.path().join("run"))
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_CONFIG");
    }

    fn docker(&self, args: &[&str]) -> Output {
        let mut c = Command::new(self.path().join("bin/docker"));
        c.args(args);
        self.env(&mut c);
        let out = c.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        out
    }

    fn podbag(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_podbag"));
        c.args(["system", "docker"]).args(args);
        self.env(&mut c);
        c.output().unwrap()
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.path().join("docker-calls")).unwrap_or_default()
    }

    fn current(&self) -> String {
        String::from_utf8_lossy(&self.docker(&["context", "show"]).stdout)
            .trim()
            .to_string()
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// podbag's socket for this HOME.
fn podbag_socket(home: &Home) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.path().join("mac/run/docker.sock")
    } else {
        home.path().join("run/podbag/docker.sock")
    }
}

#[test]
fn use_switches_docker_to_podbag_and_restore_goes_back() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root's socket is elsewhere
    }
    let home = Home::new();
    let out = home.podbag(&["status"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("docker → Docker Desktop (context desktop-linux"),
        "{}",
        text(&out)
    );
    assert!(
        text(&out).contains("podbag system docker use"),
        "{}",
        text(&out)
    );

    let out = home.podbag(&["use"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("docker → podbag"), "{}", text(&out));
    assert!(
        text(&out).contains("goes back to desktop-linux"),
        "{}",
        text(&out)
    );
    assert_eq!(home.current(), "podbag");
    let meta =
        String::from_utf8_lossy(&home.docker(&["context", "inspect", "podbag"]).stdout).to_string();
    assert!(
        meta.contains(&format!("unix://{}", podbag_socket(&home).display())),
        "{meta}"
    );
    let out = home.podbag(&["status"]);
    assert!(text(&out).contains("docker → podbag"), "{}", text(&out));
    // Again: the context is there already, so it is not made again.
    let _ = std::fs::remove_file(home.path().join("docker-calls"));
    assert!(home.podbag(&["use"]).status.success());
    assert!(!home.calls().contains("context create"), "{}", home.calls());
    // Other contexts are never changed or removed.
    assert!(!home.calls().contains("rm"), "{}", home.calls());

    let out = home.podbag(&["restore"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("docker → context desktop-linux"),
        "{}",
        text(&out)
    );
    assert_eq!(home.current(), "desktop-linux");
}

#[test]
fn docker_host_in_the_shell_is_named() {
    let home = Home::new();
    let mut c = Command::new(env!("CARGO_BIN_EXE_podbag"));
    c.args(["system", "docker", "status"]);
    home.env(&mut c);
    c.env("DOCKER_HOST", "unix:///elsewhere/docker.sock");
    let out = c.output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("DOCKER_HOST is set in this shell"),
        "{}",
        text(&out)
    );
}

#[test]
fn without_a_docker_cli_it_says_so() {
    let home = Home::new();
    std::fs::remove_file(home.path().join("bin/docker")).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_podbag"));
    c.args(["system", "docker", "use"]);
    home.env(&mut c);
    // Nowhere to find one, also not in the usual folders.
    c.env("PATH", home.path().join("bin"));
    let out = c.output().unwrap();
    if Path::new("/usr/local/bin/docker").exists() || Path::new("/opt/homebrew/bin/docker").exists()
    {
        return; // this machine has one in a folder that is always searched
    }
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("docker CLI is not installed"),
        "{}",
        text(&out)
    );
}
