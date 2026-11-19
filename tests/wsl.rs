//! What podbag.exe needs that every platform can test: the WSL distribution's files,
//! Windows paths as WSL sees them, and what `wsl.exe` prints. (A separate test file, so
//! that it also builds and runs on Windows.)

use podbag::wsl::*;

#[test]
fn drive_paths_go_under_mnt() {
    assert_eq!(wsl_path(r"C:\Users\me\app").unwrap(), "/mnt/c/Users/me/app");
    assert_eq!(wsl_path("D:/data/x/").unwrap(), "/mnt/d/data/x");
    assert_eq!(wsl_path(r"e:\").unwrap(), "/mnt/e");
}

#[test]
fn relative_and_linux_paths_keep_their_place() {
    assert_eq!(wsl_path(r".\data\x").unwrap(), "./data/x");
    assert_eq!(wsl_path(r"data\x").unwrap(), "data/x");
    assert_eq!(wsl_path("/var/lib/x").unwrap(), "/var/lib/x");
}

#[test]
fn network_and_drive_relative_paths_are_refused() {
    assert!(wsl_path(r"\\server\share\x").is_err());
    assert!(wsl_path("C:").is_err());
    assert!(wsl_path("C:data").is_err());
}

#[test]
fn volume_sources_from_windows_become_wsl_paths() {
    assert_eq!(
        wsl_volume(r"C:\Users\me\app:/app:ro").unwrap(),
        "/mnt/c/Users/me/app:/app:ro"
    );
    assert_eq!(wsl_volume("D:/data:/data").unwrap(), "/mnt/d/data:/data");
    assert_eq!(wsl_volume(r".\data:/data").unwrap(), "./data:/data");
    assert_eq!(wsl_volume("/srv:/srv").unwrap(), "/srv:/srv");
}

#[test]
fn named_and_anonymous_volumes_stay() {
    assert_eq!(wsl_volume("data:/data").unwrap(), "data:/data");
    assert_eq!(wsl_volume("/cache").unwrap(), "/cache");
}

#[test]
fn a_windows_volume_needs_a_target_and_a_drive() {
    assert!(wsl_volume(r"C:\data").is_err());
    assert!(wsl_volume(r"\\server\share:/data").is_err());
}

#[test]
fn distro_names_read_utf16_and_utf8() {
    let utf16: Vec<u8> = "\u{feff}Ubuntu\r\npodbag\r\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(distro_names(&utf16), ["Ubuntu", "podbag"]);
    assert_eq!(
        distro_names(b"podbag\nUbuntu-24.04\n"),
        ["podbag", "Ubuntu-24.04"]
    );
    assert!(distro_names(b"").is_empty());
}

#[test]
fn the_distribution_holds_the_engine_and_wsl_conf() {
    let engine = b"\x7fELF engine";
    let tar = distro_tar(engine, "1.2.3").unwrap();
    let mut archive = tar::Archive::new(tar.as_slice());
    let mut seen = std::collections::HashMap::new();
    for entry in archive.entries().unwrap() {
        let mut e = entry.unwrap();
        let path = e.path().unwrap().to_string_lossy().into_owned();
        let mode = e.header().mode().unwrap();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut data).unwrap();
        seen.insert(path, (mode, data));
    }
    assert_eq!(
        seen["usr/local/bin/podbag"],
        (0o755, engine.to_vec()),
        "the engine"
    );
    assert_eq!(seen["tmp"].0, 0o1777);
    let conf = String::from_utf8(seen["etc/wsl.conf"].1.clone()).unwrap();
    assert!(conf.contains("systemd = false") && conf.contains("default = root"));
    let release = String::from_utf8(seen["etc/os-release"].1.clone()).unwrap();
    assert!(release.contains("VERSION_ID=1.2.3"), "{release}");
    assert!(String::from_utf8_lossy(&seen["etc/passwd"].1).starts_with("root:x:0:0:"));
}

#[cfg(unix)]
#[test]
fn install_self_replaces_the_engine_in_one_step() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("podbag");
    std::fs::write(&dest, b"\x7fELF old").unwrap();
    install_self(&dest, &mut &b"\x7fELF new"[..]).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"\x7fELF new");
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!dir.path().join("podbag.new").exists());
}

#[cfg(unix)]
#[test]
fn install_self_refuses_what_is_not_a_program() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("podbag");
    std::fs::write(&dest, b"\x7fELF old").unwrap();
    assert!(install_self(&dest, &mut &b"<html>"[..]).is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"\x7fELF old");
}
