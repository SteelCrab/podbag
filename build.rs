//! With the `embed-guest` feature on a macOS target, the binary carries the VM's guest
//! files: this script names their directory and an id for
//! them, which `src/macos/embedded.rs` uses.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PODBAG_GUEST_DIR");
    let embed = std::env::var_os("CARGO_FEATURE_EMBED_GUEST").is_some();
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    if !embed || !macos {
        return;
    }
    let dir = std::env::var_os("PODBAG_GUEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("target/vm")
        });
    // FNV-1a over the files' names and bytes: a new guest gets a directory of its own.
    let mut id: u64 = 0xcbf2_9ce4_8422_2325;
    for name in ["Image.zst", "initramfs.gz", "disk.zst"] {
        let path = dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "embed-guest: cannot read {}: {e}; build the guest first: \
                 dev/vm.sh run dev/macos/build-guest.sh",
                path.display()
            )
        });
        for b in name.bytes().chain(bytes) {
            id ^= u64::from(b);
            id = id.wrapping_mul(0x0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=PODBAG_GUEST_DIR={}", dir.display());
    println!("cargo:rustc-env=PODBAG_GUEST_ID={id:016x}");
}
