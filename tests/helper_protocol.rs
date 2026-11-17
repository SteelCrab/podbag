#![cfg(unix)]
use openpod::{ErrorKind, helper};
use std::process::Stdio;
use std::sync::OnceLock;

static TMP: OnceLock<tempfile::TempDir> = OnceLock::new();

fn setup() {
    TMP.get_or_init(|| {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: every test sets the same values once, before any test reads them.
        unsafe {
            std::env::set_var("OPENPOD_BIN", env!("CARGO_BIN_EXE_openpod"));
            std::env::set_var("OPENPOD_DATA_DIR", tmp.path().join("data"));
            std::env::set_var("OPENPOD_RUNTIME_DIR", tmp.path().join("run"));
        }
        tmp
    });
}

#[test]
fn shim_reports_a_missing_container() {
    setup();
    let id = "0123abcd".repeat(8);
    let mut h = helper::spawn_shim(&id, Stdio::null(), Stdio::null(), Stdio::null()).unwrap();
    let e = h.wait_started().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert_eq!(e.what, format!("cannot load container {id}"));
}

#[test]
fn shim_rejects_an_invalid_id_without_deleting_anything() {
    setup();
    let data = openpod::paths::dirs().unwrap().data;
    let keep = data.join("keep");
    std::fs::create_dir_all(&keep).unwrap();
    // "containers/.." is the data directory itself.
    let mut h = helper::spawn_shim("..", Stdio::null(), Stdio::null(), Stdio::null()).unwrap();
    assert_eq!(h.wait_started().unwrap_err().kind, ErrorKind::InvalidInput);
    assert!(keep.exists(), "the shim deleted the data directory");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn shim_reports_unsupported_and_removes_the_container_directory() {
    use openpod::{paths, spec::ContainerSpec, state};
    setup();
    let dirs = paths::dirs().unwrap();
    let spec = ContainerSpec {
        id: "f".repeat(64),
        rootfs: openpod::spec::Rootfs::Dir { path: "/".into() },
        command: vec!["true".into()],
        env: vec![],
        workdir: None,
        hostname: "box".into(),
        user: None,
        image: None,
    };
    state::write_config(&dirs, &spec).unwrap();
    let mut h = helper::spawn_shim(&spec.id, Stdio::null(), Stdio::null(), Stdio::null()).unwrap();
    let e = h.wait_started().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert_eq!(e.what, "cannot run containers on this operating system");
    assert!(!dirs.container_dir(&spec.id).exists());
}

#[test]
fn a_helper_that_dies_silently_is_an_internal_error() {
    setup();
    // `openpod --version` writes nothing to fd 3 and exits.
    let e = helper::spawn_with_args(&["--version"], Stdio::null(), Stdio::null(), Stdio::null())
        .unwrap()
        .wait_started()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Internal);
    assert_eq!(
        e.what,
        "the openpod helper exited without reporting a status"
    );
}
