//! `podbag check`: is this system ready for podbag? Facts are gathered on
//! Linux; the verdicts are plain functions of them, testable anywhere.

use std::path::Path;

/// What `check` looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Major and minor kernel version.
    pub kernel: (u32, u32),
    pub cgroup2: bool,
    pub root: bool,
    pub newuidmap: bool,
    pub newgidmap: bool,
    /// Subordinate IDs for this user in /etc/subuid and /etc/subgid.
    pub subuid: Option<u64>,
    pub subgid: Option<u64>,
    pub pasta: bool,
    /// A systemd user manager and systemd-run (for scopes: limits and the memory budget).
    pub systemd_user: bool,
    /// `kernel.apparmor_restrict_unprivileged_userns=1`.
    pub apparmor_restricts_userns: bool,
    /// podbag runs under an AppArmor profile (one that should allow `userns`).
    pub apparmor_profile: bool,
    /// Lingering is on for this user; `None` without systemd.
    pub linger: Option<bool>,
    /// `net.ipv4.ip_unprivileged_port_start`.
    pub unprivileged_port_start: Option<u32>,
    /// Docker runs here: its firewall (iptables FORWARD policy DROP) drops forwarded
    /// traffic that its own rules do not allow.
    pub docker: bool,
    /// The `image_mount` setting.
    pub image_mount: crate::config::ImageMount,
    /// As root: whether the composefs probe passed, and why not. None when rootless.
    pub composefs: Option<Result<(), String>>,
    /// The `keeper` setting.
    pub keeper: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    NotApplicable,
}

impl Level {
    pub fn label(&self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::NotApplicable => "n/a",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub level: Level,
    /// One line: what was found.
    pub detail: String,
    /// What to do, for a warning or a failure.
    pub fix: Option<String>,
}

const MIN_KERNEL: (u32, u32) = (5, 12);
const MIN_SUBIDS: u64 = 65536;

fn item(name: &str, level: Level, detail: impl Into<String>, fix: Option<&str>) -> Item {
    Item {
        name: name.to_string(),
        level,
        detail: detail.into(),
        fix: fix.map(String::from),
    }
}

/// The verdict for each item, in the order shown.
pub fn evaluate(f: &Facts) -> Vec<Item> {
    use Level::*;
    let mut items = Vec::new();
    let (major, minor) = f.kernel;
    items.push(if f.kernel >= MIN_KERNEL {
        item("kernel", Ok, format!("{major}.{minor}"), None)
    } else {
        item(
            "kernel",
            Fail,
            format!("{major}.{minor}; podbag needs 5.12 or later"),
            Some("upgrade the kernel to 5.12 or later"),
        )
    });
    items.push(if f.cgroup2 {
        item("cgroup v2", Ok, "mounted at /sys/fs/cgroup", None)
    } else {
        item(
            "cgroup v2",
            Warn,
            "not mounted: --memory, --cpus, --pids-limit and the memory budget are refused",
            Some("boot with systemd.unified_cgroup_hierarchy=1 (cgroup v2 only)"),
        )
    });
    items.push(image_mount(f));
    items.push(keeper(f));
    let na = |name: &str| item(name, NotApplicable, "not needed as root", None);
    if f.root {
        for name in [
            "newuidmap",
            "newgidmap",
            "subuid",
            "subgid",
            "pasta",
            "systemd user session",
            "apparmor",
            "linger",
            "unprivileged ports",
        ] {
            items.push(na(name));
        }
        items.push(if f.docker {
            item(
                "docker firewall",
                Warn,
                "Docker's firewall drops forwarded traffic: bridge containers cannot reach other hosts",
                Some("sudo iptables -I DOCKER-USER -i podbag0 -j ACCEPT; sudo iptables -I DOCKER-USER -o podbag0 -j ACCEPT; and the same with pbg-+ for other networks"),
            )
        } else {
            item("docker firewall", Ok, "Docker is not running", None)
        });
        return items;
    }
    for (name, found) in [("newuidmap", f.newuidmap), ("newgidmap", f.newgidmap)] {
        items.push(if found {
            item(name, Ok, "found in PATH", None)
        } else {
            item(
                name,
                Fail,
                "not found in PATH; rootless containers need it to map user IDs",
                Some("sudo apt install uidmap (Debian/Ubuntu) or sudo dnf install shadow-utils (Fedora)"),
            )
        });
    }
    for (name, count) in [("subuid", f.subuid), ("subgid", f.subgid)] {
        let fix =
            Some("sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER");
        items.push(match count {
            Some(n) if n >= MIN_SUBIDS => item(name, Ok, format!("{n} IDs in /etc/{name}"), None),
            Some(n) => item(
                name,
                Fail,
                format!("{n} IDs in /etc/{name}; podbag needs {MIN_SUBIDS}"),
                fix,
            ),
            None => item(
                name,
                Fail,
                format!("no entry for this user in /etc/{name}"),
                fix,
            ),
        });
    }
    items.push(if f.pasta {
        item("pasta", Ok, "found (bridge networks)", None)
    } else {
        item(
            "pasta",
            Fail,
            "not found; rootless bridge networks (the default) need it",
            Some("sudo apt install passt (Debian/Ubuntu) or sudo dnf install passt (Fedora); without it, run with --network host or --network none"),
        )
    });
    items.push(if f.systemd_user {
        item(
            "systemd user session",
            Ok,
            "found (limits and the memory budget)",
            None,
        )
    } else {
        item(
            "systemd user session",
            Warn,
            "not reachable: containers run, but --memory, --cpus and --pids-limit are refused",
            Some(
                "log in directly (ssh or a desktop session, not su or sudo) so systemd --user runs",
            ),
        )
    });
    items.push(match (f.apparmor_restricts_userns, f.apparmor_profile) {
        (false, _) => item("apparmor", Ok, "user namespaces are not restricted", None),
        (true, true) => item("apparmor", Ok, "podbag runs under its own profile", None),
        (true, false) => item(
            "apparmor",
            Fail,
            "user namespaces are restricted (kernel.apparmor_restrict_unprivileged_userns=1) and podbag has no profile",
            Some("podbag check --apparmor-profile | sudo tee /etc/apparmor.d/podbag && sudo apparmor_parser -r /etc/apparmor.d/podbag"),
        ),
    });
    items.push(match f.linger {
        Some(true) => item(
            "linger",
            Ok,
            "on: containers keep running after you log out",
            None,
        ),
        Some(false) => item(
            "linger",
            Warn,
            "off: containers may stop when you log out",
            Some("loginctl enable-linger $USER"),
        ),
        None => item("linger", NotApplicable, "no systemd", None),
    });
    items.push(match f.unprivileged_port_start {
        Some(0) => item(
            "unprivileged ports",
            Ok,
            "any host port can be published",
            None,
        ),
        Some(n) => item(
            "unprivileged ports",
            Ok,
            format!("host ports below {n} cannot be published without root"),
            None,
        ),
        None => item("unprivileged ports", NotApplicable, "unknown", None),
    });
    items.push(item(
        "docker firewall",
        NotApplicable,
        "rootless networks do not pass the host's firewall",
        None,
    ));
    items
}

/// How containers get their images: only root mode can use composefs.
fn image_mount(f: &Facts) -> Item {
    use Level::*;
    let name = "image mount";
    match (&f.composefs, f.image_mount) {
        (None, _) => item(
            name,
            NotApplicable,
            "rootless mode uses hard-link trees",
            None,
        ),
        (Some(_), crate::config::ImageMount::Hardlink) => item(
            name,
            Ok,
            "hard-link trees (image_mount = \"hardlink\")",
            None,
        ),
        (Some(Result::Ok(())), _) => item(name, Ok, "composefs", None),
        (Some(Err(why)), _) => item(
            name,
            Ok,
            format!(
                "hard-link trees: composefs is not available: {why}; podbag tries again after a \
                 day (to try now, delete mount-method.json in the image store)"
            ),
            None,
        ),
    }
}

/// Who watches detached containers. Never a failure: both ways work.
fn keeper(f: &Facts) -> Item {
    let detail = if f.keeper {
        "one process watches all detached containers and leaves after the last one"
    } else {
        "off: every container has a shim of its own (keeper = false)"
    };
    item("keeper", Level::Ok, detail, None)
}

/// 1 when a required item fails, else 0.
pub fn exit_code(items: &[Item]) -> i32 {
    i32::from(items.iter().any(|i| i.level == Level::Fail))
}

/// `6.17.0-5-generic` → (6, 17).
pub fn parse_kernel(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.trim().split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The AppArmor profile that lets `binary` use user namespaces (README, "AppArmor").
pub fn apparmor_profile(binary: &Path) -> String {
    format!(
        "abi <abi/4.0>,\ninclude <tunables/global>\n\nprofile podbag {} flags=(unconfined) {{\n  userns,\n}}\n",
        binary.display()
    )
}

/// Look at this system.
#[cfg(target_os = "linux")]
pub fn gather() -> Facts {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let in_path = |tool: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(tool).is_file()))
    };
    // SAFETY: getuid/geteuid have no preconditions.
    let (uid, root) = unsafe { (libc::getuid(), libc::geteuid() == 0) };
    // The same user the run looks up (directory users included).
    let user = crate::idmap::this_user(uid);
    let subids =
        |file: &str| crate::idmap::parse_subid(&read(file), &user, uid).map(|r| u64::from(r.count));
    let linger_dir = Path::new("/var/lib/systemd/linger");
    let config = crate::paths::dirs()
        .and_then(|d| crate::config::load(&d))
        .ok();
    Facts {
        kernel: parse_kernel(&read("/proc/sys/kernel/osrelease")).unwrap_or((0, 0)),
        cgroup2: Path::new("/sys/fs/cgroup/cgroup.controllers").exists(),
        root,
        newuidmap: in_path("newuidmap"),
        newgidmap: in_path("newgidmap"),
        subuid: subids("/etc/subuid"),
        subgid: subids("/etc/subgid"),
        pasta: crate::linux::session::pasta().is_some(),
        systemd_user: crate::cgroup::user_manager() && crate::cgroup::systemd_run().is_some(),
        apparmor_restricts_userns: read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .trim()
            == "1",
        apparmor_profile: {
            let label = crate::linux::apparmor_label(None);
            !label.is_empty() && !label.starts_with("unconfined")
        },
        linger: (linger_dir.is_dir() || Path::new("/run/systemd/system").is_dir())
            .then(|| linger_dir.join(&user).exists()),
        unprivileged_port_start: read("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .trim()
            .parse()
            .ok(),
        docker: Path::new("/sys/class/net/docker0").exists()
            || Path::new("/run/docker.sock").exists(),
        image_mount: config.as_ref().map(|c| c.image_mount).unwrap_or_default(),
        composefs: root.then(composefs_support),
        keeper: config.map_or(crate::config::KEEPER_DEFAULT, |c| c.keeper),
    }
}

/// The cached composefs probe result for the root store (probing when there is none).
#[cfg(target_os = "linux")]
fn composefs_support() -> Result<(), String> {
    let dirs = crate::paths::dirs().map_err(|e| e.to_string())?;
    let store = crate::store::Store::open(&dirs, true).map_err(|e| e.to_string())?;
    let found = store.composefs_support();
    if found.composefs {
        Ok(())
    } else {
        Err(found.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_rootless() -> Facts {
        Facts {
            kernel: (6, 8),
            cgroup2: true,
            root: false,
            newuidmap: true,
            newgidmap: true,
            subuid: Some(65536),
            subgid: Some(65536),
            pasta: true,
            systemd_user: true,
            apparmor_restricts_userns: true,
            apparmor_profile: true,
            linger: Some(true),
            unprivileged_port_start: Some(1024),
            docker: false,
            image_mount: crate::config::ImageMount::Auto,
            composefs: None,
            keeper: true,
        }
    }

    #[test]
    fn the_keeper_item_says_who_watches() {
        let on = evaluate(&good_rootless());
        let i = item(&on, "keeper");
        assert_eq!(i.level, Level::Ok);
        assert!(
            i.detail
                .starts_with("one process watches all detached containers")
        );
        let off = evaluate(&Facts {
            keeper: false,
            ..good_rootless()
        });
        let i = item(&off, "keeper");
        assert_eq!(i.level, Level::Ok, "off is a choice, not a fault");
        assert!(
            i.detail
                .starts_with("off: every container has a shim of its own")
        );
        // Root mode shows it too.
        let root = evaluate(&Facts {
            root: true,
            ..good_rootless()
        });
        assert_eq!(item(&root, "keeper").level, Level::Ok);
    }

    #[test]
    fn image_mount_item() {
        use crate::config::ImageMount;
        let rootless = evaluate(&good_rootless());
        assert_eq!(item(&rootless, "image mount").level, Level::NotApplicable);
        let root = |composefs: Result<(), String>, setting: ImageMount| {
            let mut f = good_rootless();
            f.root = true;
            f.composefs = Some(composefs);
            f.image_mount = setting;
            item(&evaluate(&f), "image mount").clone()
        };
        let ok = root(Ok(()), ImageMount::Auto);
        assert_eq!((ok.level, ok.detail.as_str()), (Level::Ok, "composefs"));
        let old = root(
            Err("overlayfs refused data-only layers (Linux 6.1)".into()),
            ImageMount::Auto,
        );
        assert_eq!(old.level, Level::Ok);
        assert!(old.detail.contains("hard-link trees"), "{}", old.detail);
        assert!(old.detail.contains("Linux 6.1"), "{}", old.detail);
        assert!(old.detail.contains("mount-method.json"), "{}", old.detail);
        let set = root(Ok(()), ImageMount::Hardlink);
        assert_eq!(set.level, Level::Ok);
        assert_eq!(set.detail, "hard-link trees (image_mount = \"hardlink\")");
    }

    fn item<'a>(items: &'a [Item], name: &str) -> &'a Item {
        items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no item {name}: {items:?}"))
    }

    #[test]
    fn a_ready_rootless_host() {
        let items = evaluate(&good_rootless());
        for name in [
            "kernel",
            "cgroup v2",
            "newuidmap",
            "newgidmap",
            "subuid",
            "subgid",
            "pasta",
            "systemd user session",
            "apparmor",
            "linger",
            "unprivileged ports",
        ] {
            assert_eq!(item(&items, name).level, Level::Ok, "{name}: {items:?}");
        }
        assert_eq!(exit_code(&items), 0);
        assert!(item(&items, "kernel").detail.contains("6.8"));
        assert!(item(&items, "unprivileged ports").detail.contains("1024"));
    }

    #[test]
    fn required_items_fail_with_a_fix() {
        let cases: [(&str, fn(&mut Facts), &str); 7] = [
            ("kernel", |f| f.kernel = (5, 11), "5.12"),
            ("newuidmap", |f| f.newuidmap = false, "uidmap"),
            ("newgidmap", |f| f.newgidmap = false, "uidmap"),
            ("subuid", |f| f.subuid = None, "usermod"),
            ("subgid", |f| f.subgid = Some(1000), "usermod"),
            ("pasta", |f| f.pasta = false, "passt"),
            (
                "apparmor",
                |f| f.apparmor_profile = false,
                "--apparmor-profile",
            ),
        ];
        for (name, change, fix) in cases {
            let mut f = good_rootless();
            change(&mut f);
            let items = evaluate(&f);
            let i = item(&items, name);
            assert_eq!(i.level, Level::Fail, "{name}: {i:?}");
            let text = i.fix.as_deref().unwrap_or("");
            assert!(text.contains(fix), "{name}: {text}");
            assert_eq!(exit_code(&items), 1, "{name}");
        }
        // pasta: the other network modes still work.
        let mut f = good_rootless();
        f.pasta = false;
        let items = evaluate(&f);
        assert!(
            item(&items, "pasta")
                .fix
                .as_deref()
                .unwrap()
                .contains("--network host")
        );
    }

    #[test]
    fn optional_items_warn() {
        let cases: [(&str, fn(&mut Facts)); 3] = [
            ("cgroup v2", |f| f.cgroup2 = false),
            ("systemd user session", |f| f.systemd_user = false),
            ("linger", |f| f.linger = Some(false)),
        ];
        for (name, change) in cases {
            let mut f = good_rootless();
            change(&mut f);
            let items = evaluate(&f);
            let i = item(&items, name);
            assert_eq!(i.level, Level::Warn, "{name}: {i:?}");
            assert!(i.fix.is_some(), "{name}");
            assert_eq!(exit_code(&items), 0, "{name}");
        }
    }

    #[test]
    fn apparmor_without_the_restriction_needs_no_profile() {
        let mut f = good_rootless();
        f.apparmor_restricts_userns = false;
        f.apparmor_profile = false;
        assert_eq!(item(&evaluate(&f), "apparmor").level, Level::Ok);
    }

    #[test]
    fn root_skips_rootless_items() {
        let f = Facts {
            root: true,
            newuidmap: false,
            newgidmap: false,
            subuid: None,
            subgid: None,
            pasta: false,
            systemd_user: false,
            apparmor_profile: false,
            linger: None,
            unprivileged_port_start: None,
            ..good_rootless()
        };
        let items = evaluate(&f);
        for name in [
            "newuidmap",
            "newgidmap",
            "subuid",
            "subgid",
            "pasta",
            "systemd user session",
            "apparmor",
            "linger",
            "unprivileged ports",
        ] {
            assert_eq!(item(&items, name).level, Level::NotApplicable, "{name}");
        }
        assert_eq!(item(&items, "kernel").level, Level::Ok);
        assert_eq!(exit_code(&items), 0);
    }

    #[test]
    fn docker_blocks_root_bridges() {
        let root = Facts {
            root: true,
            docker: true,
            ..good_rootless()
        };
        let items = evaluate(&root);
        let i = item(&items, "docker firewall");
        assert_eq!(i.level, Level::Warn, "{i:?}");
        let fix = i.fix.as_deref().unwrap();
        assert!(
            fix.contains("DOCKER-USER") && fix.contains("podbag0") && fix.contains("pbg-+"),
            "{fix}"
        );
        assert_eq!(exit_code(&items), 0);
        // Without Docker, fine; rootless networks do not pass the host's firewall.
        let items = evaluate(&Facts {
            docker: false,
            ..root
        });
        assert_eq!(item(&items, "docker firewall").level, Level::Ok);
        let items = evaluate(&Facts {
            docker: true,
            ..good_rootless()
        });
        assert_eq!(item(&items, "docker firewall").level, Level::NotApplicable);
    }

    #[test]
    fn kernel_release_parsing() {
        assert_eq!(parse_kernel("6.17.0-5-generic"), Some((6, 17)));
        assert_eq!(parse_kernel("5.12"), Some((5, 12)));
        assert_eq!(parse_kernel("7.0.0-43-generic\n"), Some((7, 0)));
        assert_eq!(parse_kernel("garbage"), None);
    }

    #[test]
    fn apparmor_profile_text() {
        let p = apparmor_profile(Path::new("/opt/bin/podbag"));
        assert!(
            p.contains("profile podbag /opt/bin/podbag flags=(unconfined)"),
            "{p}"
        );
        assert!(p.contains("  userns,"), "{p}");
        assert!(p.starts_with("abi <abi/4.0>,"), "{p}");
    }
}
