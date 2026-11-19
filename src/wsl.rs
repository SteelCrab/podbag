//! What podbag on Windows needs that can be built and tested anywhere: the files of its
//! WSL2 distribution, Windows paths as WSL sees them, and what `wsl.exe` prints.
//!
//! On Windows, podbag.exe keeps a WSL2 distribution of its own, `podbag`, whose only
//! program is the Linux engine; every command runs there with `wsl.exe --exec`.

use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::path::Path;

/// The name of podbag's WSL distribution.
pub const DISTRO: &str = "podbag";
/// Where the Linux engine lives in the distribution.
pub const ENGINE: &str = "/usr/local/bin/podbag";

/// The distribution's /etc/wsl.conf: no systemd, no Windows programs or PATH inside, and
/// the Windows drives under /mnt for the paths of `-v`.
const WSL_CONF: &str = "\
[boot]
systemd = false

[automount]
enabled = true
root = /mnt/
mountFsTab = false

[interop]
enabled = false
appendWindowsPath = false

[network]
hostname = podbag
generateHosts = true
generateResolvConf = true

[user]
default = root
";

/// The distribution's root file system as a tar archive: the Linux engine and the few
/// files WSL reads.
pub fn distro_tar(engine: &[u8], version: &str) -> io::Result<Vec<u8>> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, mode) in [
        ("dev", 0o755),
        ("etc", 0o755),
        ("home", 0o755),
        ("mnt", 0o755),
        ("opt", 0o755),
        ("proc", 0o555),
        ("root", 0o700),
        ("run", 0o755),
        ("sys", 0o555),
        ("tmp", 0o1777),
        ("usr", 0o755),
        ("usr/local", 0o755),
        ("usr/local/bin", 0o755),
        ("var", 0o755),
        ("var/lib", 0o755),
        ("var/log", 0o755),
        ("var/tmp", 0o1777),
    ] {
        append(&mut b, tar::EntryType::Directory, path, mode, &[])?;
    }
    let os_release =
        format!("NAME=podbag\nID=podbag\nVERSION_ID={version}\nPRETTY_NAME=\"podbag {version}\"\n");
    let passwd = format!("root:x:0:0:root:/root:{ENGINE}\n");
    for (path, data) in [
        ("etc/group", "root:x:0:\n".as_bytes()),
        ("etc/os-release", os_release.as_bytes()),
        ("etc/passwd", passwd.as_bytes()),
        ("etc/wsl.conf", WSL_CONF.as_bytes()),
    ] {
        append(&mut b, tar::EntryType::Regular, path, 0o644, data)?;
    }
    append(&mut b, tar::EntryType::Regular, &ENGINE[1..], 0o755, engine)?;
    b.into_inner()
}

fn append(
    b: &mut tar::Builder<Vec<u8>>,
    kind: tar::EntryType,
    path: &str,
    mode: u32,
    data: &[u8],
) -> io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_size(data.len() as u64);
    b.append_data(&mut h, path, data)
}

/// A Windows path as WSL sees it: `C:\Users\me` is `/mnt/c/Users/me`. A relative path
/// keeps its place (the engine starts in the Windows working directory), with `/` for
/// `\`; a Linux path (`/data`) is one in the distribution. Network paths
/// (`\\server\share`) have no WSL form.
pub fn wsl_path(path: &str) -> Result<String, String> {
    if path.starts_with(r"\\") || path.starts_with("//") {
        return Err(format!(
            "{path}: network paths cannot be used; copy the files to a drive first"
        ));
    }
    let b = path.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        if b.len() == 2 || (b[2] != b'\\' && b[2] != b'/') {
            return Err(format!(
                "{path}: give the full path, as in {}\\{}",
                &path[..2],
                &path[2..]
            ));
        }
        let rest = path[2..].replace('\\', "/");
        return Ok(format!(
            "/mnt/{}{}",
            char::from(b[0]).to_ascii_lowercase(),
            rest.trim_end_matches('/')
        ));
    }
    Ok(path.replace('\\', "/"))
}

/// `-v SOURCE:TARGET[:OPTIONS]` with a Windows source as WSL sees it. Named volumes
/// (`data:/data`) and paths in the distribution stay as they are.
pub fn wsl_volume(spec: &str) -> Result<String, String> {
    let b = spec.as_bytes();
    let drive = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    // The source ends at the first ':' after the drive's.
    let from = if drive { 2 } else { 0 };
    let Some(end) = spec[from..].find(':').map(|i| i + from) else {
        return if drive || spec.contains('\\') {
            Err(format!(
                "{spec}: give a place in the container too, as in {spec}:/data"
            ))
        } else {
            // `-v /dir`: an anonymous volume at /dir.
            Ok(spec.to_string())
        };
    };
    let source = &spec[..end];
    if !(drive || source.starts_with('.') || source.contains(['\\', '/'])) {
        return Ok(spec.to_string());
    }
    Ok(format!("{}{}", wsl_path(source)?, &spec[end..]))
}

/// The names in what `wsl.exe --list --quiet` prints.
pub fn distro_names(output: &[u8]) -> Vec<String> {
    decode(output)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// What `wsl.exe` prints, as text: UTF-16LE (older WSL) or UTF-8 (with `WSL_UTF8=1`).
pub fn decode(output: &[u8]) -> String {
    let nuls = output
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&b| b == 0)
        .count();
    let text = if output.len() % 2 == 0 && nuls * 4 > output.len() {
        let units: Vec<u16> = output
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(output).into_owned()
    };
    text.trim_start_matches('\u{feff}').replace('\0', "")
}

/// Replace the engine at `dest` with one read from `input` (`podbag __install-self`, which
/// podbag.exe runs in its distribution when its version changes). The new program is
/// written beside the old one and renamed over it: running containers keep theirs.
#[cfg(unix)]
pub fn install_self(dest: &Path, input: &mut impl Read) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    if !bytes.starts_with(b"\x7fELF") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the input is not a Linux program",
        ));
    }
    let tmp = dest.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, dest)
}
