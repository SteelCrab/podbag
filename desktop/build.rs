//! Windows: podbag-desktop.exe gets the app icon and its version information, which
//! Explorer, the taskbar and Windows Installer read.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let manifest = var("CARGO_MANIFEST_DIR");
    let icon = Path::new(&manifest).join("icons").join("icon.ico");
    let version = var("CARGO_PKG_VERSION");
    let numbers = format!(
        "{},{},{},0",
        var("CARGO_PKG_VERSION_MAJOR"),
        var("CARGO_PKG_VERSION_MINOR"),
        var("CARGO_PKG_VERSION_PATCH")
    );
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "SteelCrab"
      VALUE "FileDescription", "Podbag Desktop"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "podbag-desktop"
      VALUE "LegalCopyright", "Apache-2.0"
      VALUE "OriginalFilename", "podbag-desktop.exe"
      VALUE "ProductName", "Podbag Desktop"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        // Resource scripts take C strings: double the backslashes of a Windows path.
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let file = Path::new(&var("OUT_DIR")).join("podbag-desktop.rc");
    std::fs::write(&file, rc).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    embed_resource::compile_for(&file, ["podbag-desktop"], embed_resource::NONE)
        .manifest_required()
        .unwrap_or_else(|e| panic!("Windows resources: {e}"));
}
