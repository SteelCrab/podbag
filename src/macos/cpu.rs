//! The Mac's CPU and the VM guest's: Virtualization.framework runs a guest of the Mac's own
//! CPU only (an arm64 kernel on Apple silicon, an x86_64 one on an Intel Mac).

use std::io::Read;
use std::path::Path;

/// Whether this process is an Intel program that Rosetta runs on Apple silicon
/// (`sysctl.proc_translated`).
pub fn translated() -> bool {
    let mut on: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    // SAFETY: sysctl.proc_translated is a 32-bit integer; the pointers name `on` and its
    // length. An Intel Mac's macOS has no such name: the call fails and `on` stays 0.
    let rc = unsafe {
        libc::sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            std::ptr::from_mut(&mut on).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    rc == 0 && on == 1
}

/// The Mac's CPU, in Rust's names (`aarch64`, `x86_64`): an Intel program under Rosetta
/// still runs on Apple silicon.
pub fn mac() -> &'static str {
    if translated() {
        "aarch64"
    } else {
        std::env::consts::ARCH
    }
}

/// The bytes [`kernel`] needs from the start of a kernel file.
const HEAD: usize = 0x210;

/// The CPU a Linux kernel file is for, from its header: an arm64 `Image` has `ARM\x64` at
/// 0x38; an x86 `bzImage` has `HdrS` at 0x202.
pub fn kernel(head: &[u8]) -> Option<&'static str> {
    if head.get(0x38..0x3c) == Some(b"ARM\x64") {
        Some("aarch64")
    } else if head.get(0x202..0x206) == Some(b"HdrS") {
        Some("x86_64")
    } else {
        None
    }
}

/// Whether the guest's kernel at `path` runs on this Mac; the error says what to use
/// instead.
pub fn check_guest(path: &Path) -> Result<(), String> {
    let mut head = Vec::with_capacity(HEAD);
    std::fs::File::open(path)
        .and_then(|f| f.take(HEAD as u64).read_to_end(&mut head))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    guest_fits(kernel(&head), mac(), translated()).map_err(|e| format!("{}: {e}", path.display()))
}

fn guest_fits(kernel: Option<&str>, mac: &str, translated: bool) -> Result<(), String> {
    if translated {
        return Err(
            "this podbag is for Intel Macs, and Rosetta runs it on Apple silicon: \
                    install podbag-aarch64-macos"
                .into(),
        );
    }
    match kernel {
        Some(cpu) if cpu == mac => Ok(()),
        Some(cpu) => Err(format!(
            "the VM's kernel is for {cpu}, and this Mac's CPU is {mac}: build the guest \
             for {mac}"
        )),
        None => Err("this is not a Linux kernel that the VM can start".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm64_image() -> Vec<u8> {
        let mut head = vec![0; HEAD];
        head[0x38..0x3c].copy_from_slice(b"ARM\x64");
        head
    }

    fn bzimage() -> Vec<u8> {
        let mut head = vec![0; HEAD];
        head[0x1fe..0x200].copy_from_slice(&[0x55, 0xaa]);
        head[0x202..0x206].copy_from_slice(b"HdrS");
        head
    }

    #[test]
    fn kernel_headers_name_their_cpu() {
        assert_eq!(kernel(&arm64_image()), Some("aarch64"));
        assert_eq!(kernel(&bzimage()), Some("x86_64"));
        assert_eq!(kernel(&[0; HEAD]), None);
        assert_eq!(kernel(b"short"), None);
    }

    #[test]
    fn a_guest_must_match_the_macs_cpu() {
        assert!(guest_fits(Some("aarch64"), "aarch64", false).is_ok());
        assert!(guest_fits(Some("x86_64"), "x86_64", false).is_ok());
        let wrong = guest_fits(Some("aarch64"), "x86_64", false).unwrap_err();
        assert!(wrong.contains("for aarch64") && wrong.contains("build the guest for x86_64"));
        assert!(guest_fits(None, "x86_64", false).is_err());
    }

    #[test]
    fn an_intel_build_under_rosetta_names_the_apple_silicon_build() {
        let e = guest_fits(Some("x86_64"), "aarch64", true).unwrap_err();
        assert!(e.contains("podbag-aarch64-macos"), "{e}");
    }

    #[test]
    fn a_kernel_file_is_checked_against_this_mac() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Image");
        let (fits, other) = if mac() == "aarch64" {
            (arm64_image(), bzimage())
        } else {
            (bzimage(), arm64_image())
        };
        std::fs::write(&path, fits).unwrap();
        assert_eq!(check_guest(&path).is_ok(), !translated());
        std::fs::write(&path, other).unwrap();
        assert!(check_guest(&path).is_err());
        assert!(check_guest(&dir.path().join("missing")).is_err());
    }
}
