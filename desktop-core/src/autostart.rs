//! Launch at login: the same entries as tauri-plugin-autostart 2 makes through
//! auto-launch 0.6 (both MIT/Apache-2.0, https://github.com/tauri-apps/plugins-workspace
//! and https://github.com/zzzgydi/auto-launch), so the setting carries over:
//!
//! - macOS: a Launch Agent, `~/Library/LaunchAgents/podbag.plist`, that runs the app's
//!   executable at load.
//! - Linux: `~/.config/autostart/podbag.desktop`, running the AppImage or the
//!   executable.
//! - Windows: the value `podbag` (`Podbag Desktop` for the MSI) under
//!   `Software\Microsoft\Windows\CurrentVersion\Run`, in HKLM when the app may write
//!   there and in HKCU otherwise, with Task Manager's `StartupApproved` switch.
//!
//! The `podbag startup` command of the CLI manages the same entry.

use std::io;
use std::path::{Path, PathBuf};

/// The entry's name: the app's product name, as the plugin takes it.
const NAME: &str = "podbag";
/// The product name of the Windows MSI, which installs into a folder of that name.
const MSI_NAME: &str = "Podbag Desktop";

/// The login entry of this app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginItem {
    /// The entry's name (file name or registry value).
    pub name: String,
    /// What runs at login.
    pub program: String,
    store: Store,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Store {
    /// A file that exists while the entry is on.
    File { path: PathBuf, contents: String },
    /// The Windows registry.
    #[cfg_attr(not(windows), allow(dead_code))]
    Registry,
}

impl LoginItem {
    /// The entry for the running app.
    pub fn for_this_app() -> Result<LoginItem, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        if cfg!(windows) {
            let name = if exe
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == MSI_NAME)
            {
                MSI_NAME
            } else {
                NAME
            };
            return Ok(LoginItem {
                name: name.into(),
                program: exe.display().to_string(),
                store: Store::Registry,
            });
        }
        let home = home_dir().ok_or("Failed to find home directory")?;
        if cfg!(target_os = "macos") {
            let exe = exe.canonicalize().map_err(|e| e.to_string())?;
            Ok(LoginItem::launch_agent(
                &home,
                NAME,
                &exe.display().to_string(),
            ))
        } else {
            let program = std::env::var("APPIMAGE")
                .ok()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| exe.display().to_string());
            Ok(LoginItem::xdg_autostart(&home, NAME, &program))
        }
    }

    /// A Launch Agent in `home`'s `Library/LaunchAgents` (macOS).
    pub fn launch_agent(home: &Path, name: &str, program: &str) -> LoginItem {
        LoginItem {
            name: name.into(),
            program: program.into(),
            store: Store::File {
                path: home
                    .join("Library/LaunchAgents")
                    .join(format!("{name}.plist")),
                contents: launch_agent_plist(name, program),
            },
        }
    }

    /// An XDG autostart entry in `home`'s `.config/autostart` (Linux).
    pub fn xdg_autostart(home: &Path, name: &str, program: &str) -> LoginItem {
        LoginItem {
            name: name.into(),
            program: program.into(),
            store: Store::File {
                path: home
                    .join(".config/autostart")
                    .join(format!("{name}.desktop")),
                contents: desktop_entry(name, program),
            },
        }
    }

    /// The file of a file entry.
    pub fn file(&self) -> Option<&Path> {
        match &self.store {
            Store::File { path, .. } => Some(path),
            Store::Registry => None,
        }
    }

    pub fn is_enabled(&self) -> Result<bool, String> {
        match &self.store {
            Store::File { path, .. } => Ok(path.exists()),
            Store::Registry => self.registry_enabled(),
        }
    }

    pub fn set_enabled(&self, on: bool) -> Result<(), String> {
        let done = match &self.store {
            Store::File { path, contents } if on => write_file(path, contents),
            Store::File { path, .. } => match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            Store::Registry => return self.registry_set(on),
        };
        done.map_err(|e| e.to_string())
    }

    #[cfg(windows)]
    fn registry_enabled(&self) -> Result<bool, String> {
        registry::is_enabled(&registry::System, &self.name).map_err(|e| e.to_string())
    }

    #[cfg(windows)]
    fn registry_set(&self, on: bool) -> Result<(), String> {
        let done = if on {
            registry::enable(&registry::System, &self.name, &self.program)
        } else {
            registry::disable(&registry::System, &self.name)
        };
        done.map_err(|e| e.to_string())
    }

    #[cfg(not(windows))]
    fn registry_enabled(&self) -> Result<bool, String> {
        Err("no registry on this system".into())
    }

    #[cfg(not(windows))]
    fn registry_set(&self, _on: bool) -> Result<(), String> {
        Err("no registry on this system".into())
    }
}

fn write_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The plist auto-launch writes, with no bundle identifiers and no arguments.
fn launch_agent_plist(name: &str, program: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n  \
         <dict>\n  \
         <key>Label</key>\n  \
         <string>{}</string>\n  \
         <key>AssociatedBundleIdentifiers</key>\n  \
         <array></array>\n  \
         <key>ProgramArguments</key>\n  \
         <array><string>{}</string></array>\n  \
         <key>RunAtLoad</key>\n  \
         <true/>\n  \
         </dict>\n\
         </plist>",
        xml_escape(name),
        xml_escape(program),
    )
}

/// The desktop entry auto-launch writes, with no arguments. A path with spaces is
/// quoted, as the desktop entry spec asks.
fn desktop_entry(name: &str, program: &str) -> String {
    let exec = if program.contains([' ', '"', '\\', '\t']) {
        let escaped = program.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        program.to_string()
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name={name}\n\
         Comment={name} startup script\n\
         Exec={exec} \n\
         StartupNotify=false\n\
         Terminal=false"
    )
}

/// The Run key logic of auto-launch's `WindowsEnableMode::Dynamic`, over a registry
/// that tests can fake.
pub mod registry {
    use std::io;

    pub const RUN: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    /// Task Manager's on/off switch for each Run value.
    pub const APPROVED: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    /// "Enabled" in `APPROVED`.
    pub const APPROVED_ON: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub enum Root {
        LocalMachine,
        CurrentUser,
    }

    /// The registry calls the entry needs. Missing keys and values are `Ok(None)`;
    /// no permission is `io::ErrorKind::PermissionDenied`.
    pub trait Registry {
        fn get_string(&self, root: Root, key: &str, name: &str) -> io::Result<Option<String>>;
        /// Creates the key if needed.
        fn set_string(&self, root: Root, key: &str, name: &str, value: &str) -> io::Result<()>;
        /// Ok if the key or the value is missing.
        fn remove(&self, root: Root, key: &str, name: &str) -> io::Result<()>;
        fn get_bytes(&self, root: Root, key: &str, name: &str) -> io::Result<Option<Vec<u8>>>;
        /// Sets a binary value in an existing key; Ok without a change if the key is missing.
        fn set_bytes_in_existing(
            &self,
            root: Root,
            key: &str,
            name: &str,
            value: &[u8],
        ) -> io::Result<()>;
    }

    /// The Run value: the program and its arguments (none), as auto-launch writes it.
    pub fn run_value(program: &str) -> String {
        format!("{program} ")
    }

    fn enable_in(reg: &dyn Registry, root: Root, name: &str, program: &str) -> io::Result<()> {
        reg.set_string(root, RUN, name, &run_value(program))?;
        reg.set_bytes_in_existing(root, APPROVED, name, &APPROVED_ON)
    }

    /// All users when allowed, else the current user.
    pub fn enable(reg: &dyn Registry, name: &str, program: &str) -> io::Result<()> {
        match enable_in(reg, Root::LocalMachine, name, program) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                enable_in(reg, Root::CurrentUser, name, program)
            }
            done => done,
        }
    }

    /// Both places; no permission for all users is no entry there.
    pub fn disable(reg: &dyn Registry, name: &str) -> io::Result<()> {
        match reg.remove(Root::LocalMachine, RUN, name) {
            Err(e) if e.kind() != io::ErrorKind::PermissionDenied => return Err(e),
            _ => {}
        }
        reg.remove(Root::CurrentUser, RUN, name)
    }

    fn approved(reg: &dyn Registry, root: Root, name: &str) -> io::Result<bool> {
        Ok(match reg.get_bytes(root, APPROVED, name)? {
            // Task Manager's "disabled" sets the last 8 bytes (a time).
            Some(v) if v.len() >= 8 => v.iter().rev().take(8).all(|b| *b == 0),
            _ => true,
        })
    }

    /// A Run value in either place, not switched off in Task Manager.
    pub fn is_enabled(reg: &dyn Registry, name: &str) -> io::Result<bool> {
        let registered = reg.get_string(Root::LocalMachine, RUN, name)?.is_some()
            || reg.get_string(Root::CurrentUser, RUN, name)?.is_some();
        Ok(registered
            && approved(reg, Root::LocalMachine, name)?
            && approved(reg, Root::CurrentUser, name)?)
    }

    /// The real registry.
    #[cfg(windows)]
    pub struct System;

    #[cfg(windows)]
    mod system {
        use super::{Registry, Root, System};
        use std::io;
        use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};

        const E_ACCESSDENIED: u32 = 0x8007_0005;
        const E_FILENOTFOUND: u32 = 0x8007_0002;

        fn root(r: Root) -> &'static Key {
            match r {
                Root::LocalMachine => LOCAL_MACHINE,
                Root::CurrentUser => CURRENT_USER,
            }
        }

        fn io(e: windows_registry::Result<()>) -> io::Result<()> {
            e.map_err(error)
        }

        fn error(e: windows_result::Error) -> io::Error {
            let kind = match e.code().0 as u32 {
                E_ACCESSDENIED => io::ErrorKind::PermissionDenied,
                E_FILENOTFOUND => io::ErrorKind::NotFound,
                _ => io::ErrorKind::Other,
            };
            io::Error::new(kind, e.message())
        }

        fn missing<T>(r: windows_registry::Result<T>) -> io::Result<Option<T>> {
            match r {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.code().0 as u32 == E_FILENOTFOUND => Ok(None),
                Err(e) => Err(error(e)),
            }
        }

        impl Registry for System {
            fn get_string(&self, r: Root, key: &str, name: &str) -> io::Result<Option<String>> {
                missing(root(r).open(key).and_then(|k| k.get_string(name)))
            }

            fn set_string(&self, r: Root, key: &str, name: &str, value: &str) -> io::Result<()> {
                io(root(r).create(key).and_then(|k| k.set_string(name, value)))
            }

            fn remove(&self, r: Root, key: &str, name: &str) -> io::Result<()> {
                let key = root(r).options().write().open(key);
                missing(key.and_then(|k| k.remove_value(name))).map(|_| ())
            }

            fn get_bytes(&self, r: Root, key: &str, name: &str) -> io::Result<Option<Vec<u8>>> {
                let value = root(r).open(key).and_then(|k| k.get_value(name));
                missing(value).map(|v| v.map(|v| v.to_vec()))
            }

            fn set_bytes_in_existing(
                &self,
                r: Root,
                key: &str,
                name: &str,
                value: &[u8],
            ) -> io::Result<()> {
                match missing(root(r).options().write().open(key))? {
                    Some(k) => io(k.set_bytes(name, Type::Bytes, value)),
                    None => Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::registry::{self, APPROVED, RUN, Registry, Root};
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn launch_agent_is_the_plugins_plist() {
        let home = tempfile::tempdir().unwrap();
        let program = "/Applications/podbag.app/Contents/MacOS/podbag-desktop";
        let item = LoginItem::launch_agent(home.path(), "podbag", program);
        let file = home.path().join("Library/LaunchAgents/podbag.plist");
        assert_eq!(item.file(), Some(file.as_path()));
        assert!(!item.is_enabled().unwrap());

        item.set_enabled(true).unwrap();
        assert!(item.is_enabled().unwrap());
        let plist = std::fs::read_to_string(&file).unwrap();
        // What auto-launch 0.6 writes for these inputs.
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
            <plist version=\"1.0\">\n  <dict>\n  <key>Label</key>\n  <string>podbag</string>\n  \
            <key>AssociatedBundleIdentifiers</key>\n  <array></array>\n  \
            <key>ProgramArguments</key>\n  \
            <array><string>/Applications/podbag.app/Contents/MacOS/podbag-desktop</string></array>\n  \
            <key>RunAtLoad</key>\n  <true/>\n  </dict>\n</plist>";
        assert_eq!(plist, expected);

        item.set_enabled(false).unwrap();
        assert!(!file.exists());
        // Off twice is fine.
        item.set_enabled(false).unwrap();
        assert!(!item.is_enabled().unwrap());
    }

    #[test]
    fn an_entry_the_plugin_made_counts_as_on() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("podbag.plist"), "made by an older app").unwrap();
        let item = LoginItem::launch_agent(home.path(), "podbag", "/new/path");
        assert!(item.is_enabled().unwrap());
        // On again rewrites it for this program.
        item.set_enabled(true).unwrap();
        let plist = std::fs::read_to_string(dir.join("podbag.plist")).unwrap();
        assert!(plist.contains("<string>/new/path</string>"));
    }

    #[test]
    fn xdg_entry_is_the_plugins_desktop_file() {
        let home = tempfile::tempdir().unwrap();
        let item = LoginItem::xdg_autostart(home.path(), "podbag", "/usr/bin/podbag-desktop");
        item.set_enabled(true).unwrap();
        let file = home.path().join(".config/autostart/podbag.desktop");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[Desktop Entry]\nType=Application\nVersion=1.0\nName=podbag\n\
             Comment=podbag startup script\nExec=/usr/bin/podbag-desktop \n\
             StartupNotify=false\nTerminal=false"
        );
        assert!(item.is_enabled().unwrap());
        item.set_enabled(false).unwrap();
        assert!(!item.is_enabled().unwrap());
    }

    #[test]
    fn xdg_entry_quotes_a_path_with_spaces() {
        let entry = desktop_entry("podbag", "/home/a b/Podbag.AppImage");
        assert!(entry.contains("Exec=\"/home/a b/Podbag.AppImage\" \n"));
    }

    /// An in-memory registry. `admin`: HKLM may be written.
    #[derive(Default)]
    struct FakeRegistry {
        admin: bool,
        values: RefCell<BTreeMap<(Root, String, String), Vec<u8>>>,
        keys: RefCell<BTreeSet<(Root, String)>>,
    }

    impl FakeRegistry {
        fn with_approved_keys(admin: bool) -> FakeRegistry {
            let r = FakeRegistry {
                admin,
                ..FakeRegistry::default()
            };
            for root in [Root::LocalMachine, Root::CurrentUser] {
                r.keys.borrow_mut().insert((root, APPROVED.into()));
            }
            r
        }

        fn writable(&self, root: Root) -> io::Result<()> {
            if root == Root::LocalMachine && !self.admin {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            Ok(())
        }

        fn string(&self, root: Root, name: &str) -> Option<String> {
            self.get_string(root, RUN, name).unwrap()
        }

        fn put(&self, root: Root, key: &str, name: &str, value: &[u8]) {
            self.keys.borrow_mut().insert((root, key.into()));
            let k = (root, key.to_string(), name.to_string());
            self.values.borrow_mut().insert(k, value.to_vec());
        }
    }

    impl Registry for FakeRegistry {
        fn get_string(&self, root: Root, key: &str, name: &str) -> io::Result<Option<String>> {
            let k = (root, key.to_string(), name.to_string());
            Ok(self
                .values
                .borrow()
                .get(&k)
                .map(|v| String::from_utf8_lossy(v).into_owned()))
        }

        fn set_string(&self, root: Root, key: &str, name: &str, value: &str) -> io::Result<()> {
            self.writable(root)?;
            self.put(root, key, name, value.as_bytes());
            Ok(())
        }

        fn remove(&self, root: Root, key: &str, name: &str) -> io::Result<()> {
            self.writable(root)?;
            let k = (root, key.to_string(), name.to_string());
            self.values.borrow_mut().remove(&k);
            Ok(())
        }

        fn get_bytes(&self, root: Root, key: &str, name: &str) -> io::Result<Option<Vec<u8>>> {
            let k = (root, key.to_string(), name.to_string());
            Ok(self.values.borrow().get(&k).cloned())
        }

        fn set_bytes_in_existing(
            &self,
            root: Root,
            key: &str,
            name: &str,
            value: &[u8],
        ) -> io::Result<()> {
            self.writable(root)?;
            if self.keys.borrow().contains(&(root, key.to_string())) {
                self.put(root, key, name, value);
            }
            Ok(())
        }
    }

    const EXE: &str = r"C:\Program Files\Podbag Desktop\podbag-desktop.exe";

    #[test]
    fn registry_entry_goes_to_the_current_user_without_admin_rights() {
        let reg = FakeRegistry::with_approved_keys(false);
        assert!(!registry::is_enabled(&reg, "Podbag Desktop").unwrap());
        registry::enable(&reg, "Podbag Desktop", EXE).unwrap();
        assert_eq!(
            reg.string(Root::CurrentUser, "Podbag Desktop").as_deref(),
            Some(r"C:\Program Files\Podbag Desktop\podbag-desktop.exe ")
        );
        assert_eq!(reg.string(Root::LocalMachine, "Podbag Desktop"), None);
        assert_eq!(
            reg.get_bytes(Root::CurrentUser, APPROVED, "Podbag Desktop")
                .unwrap(),
            Some(registry::APPROVED_ON.to_vec())
        );
        assert!(registry::is_enabled(&reg, "Podbag Desktop").unwrap());

        registry::disable(&reg, "Podbag Desktop").unwrap();
        assert!(!registry::is_enabled(&reg, "Podbag Desktop").unwrap());
        // Off twice is fine.
        registry::disable(&reg, "Podbag Desktop").unwrap();
    }

    #[test]
    fn registry_entry_goes_to_all_users_with_admin_rights() {
        let reg = FakeRegistry::with_approved_keys(true);
        registry::enable(&reg, "podbag", EXE).unwrap();
        assert!(reg.string(Root::LocalMachine, "podbag").is_some());
        assert_eq!(reg.string(Root::CurrentUser, "podbag"), None);
        assert!(registry::is_enabled(&reg, "podbag").unwrap());
        registry::disable(&reg, "podbag").unwrap();
        assert!(!registry::is_enabled(&reg, "podbag").unwrap());
    }

    #[test]
    fn task_manager_can_switch_the_entry_off() {
        let reg = FakeRegistry::default();
        reg.put(Root::CurrentUser, RUN, "podbag", b"x.exe ");
        // No StartupApproved value: on.
        assert!(registry::is_enabled(&reg, "podbag").unwrap());
        // Disabled in Task Manager: a time in the last 8 bytes.
        let off = [3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        reg.put(Root::CurrentUser, APPROVED, "podbag", &off);
        assert!(!registry::is_enabled(&reg, "podbag").unwrap());
        // Turning it on again clears the switch.
        registry::enable(&reg, "podbag", "x.exe").unwrap();
        assert!(registry::is_enabled(&reg, "podbag").unwrap());
    }

    #[test]
    fn missing_approved_key_is_not_created() {
        let reg = FakeRegistry::default();
        registry::enable(&reg, "podbag", "x.exe").unwrap();
        assert_eq!(
            reg.get_bytes(Root::CurrentUser, APPROVED, "podbag")
                .unwrap(),
            None
        );
        assert!(registry::is_enabled(&reg, "podbag").unwrap());
    }
}
