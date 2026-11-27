//! Additional desktop operations on the current terminal, including when an engine is down.
use super::{
    dialog::{Error, Result, Screen},
    preferences, resources, tutorial,
};
use podbag::{
    client::{Choice, Client, Target},
    dashboard::{cli, lab::Lab},
};
use std::path::Path;

pub enum Change {
    None,
    Notice(String),
    Engine(Choice),
    Vm(bool),
    Terminal,
    #[cfg(unix)]
    UpgradeTerminal,
    Pull,
    /// A volume was created: its name.
    Volume(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Kubernetes,
    Migration,
    Tutorials,
    Settings,
}
impl Page {
    pub const ALL: [Self; 4] = [
        Self::Kubernetes,
        Self::Migration,
        Self::Tutorials,
        Self::Settings,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Kubernetes => "nav.kubernetes",
            Self::Migration => "migration.title",
            Self::Tutorials => "nav.tutorials",
            Self::Settings => "nav.settings",
        }
    }
}

pub fn page(
    ui: &mut Screen,
    target: &Target,
    exe: &Path,
    lab: &mut Lab,
    page: Page,
) -> Result<Change> {
    match page {
        Page::Kubernetes => resources::cluster(ui, false)?,
        Page::Migration => {
            if ui.menu("migration.title", &["settings.engine", "nav.kubernetes"])? == 0 {
                resources::migrate_engine(ui, exe)?
            } else {
                resources::cluster(ui, true)?
            }
        }
        Page::Tutorials => tutorial::run(ui, lab)?,
        Page::Settings => return settings(ui, target, exe),
    }
    Ok(Change::None)
}

pub fn open(ui: &mut Screen, target: &Target, exe: &Path, lab: &mut Lab) -> Result<Change> {
    let mut keys = vec!["volume.create", "action.pull", "pull.login"];
    keys.extend(Page::ALL.map(Page::label));
    if target.choice == Choice::Podbag {
        keys.insert(2, "import.button")
    }
    let i = ui.menu("col.actions", &keys)?;
    match keys[i] {
        "volume.create" => {
            return resources::volume(ui, &Client::new(target.endpoint.clone()))
                .map(Change::Volume);
        }
        "action.pull" => return Ok(Change::Pull),
        "pull.login" => {
            let registry = resources::registry(ui, exe)?;
            if registry.login_hint().is_none() {
                return Ok(Change::Notice(
                    ui.t("pull.loggedIn").replace("{name}", &registry.label()),
                ));
            }
        }
        "import.button" => return resources::import(ui, exe, target).map(Change::Notice),
        key => {
            let selected = Page::ALL
                .into_iter()
                .find(|p| p.label() == key)
                .ok_or(Error::Back)?;
            return page(ui, target, exe, lab, selected);
        }
    }
    Ok(Change::None)
}

fn settings(ui: &mut Screen, target: &Target, exe: &Path) -> Result<Change> {
    let mut keys = vec![
        "settings.engine",
        "settings.updates",
        "settings.language",
        "settings.theme",
        "settings.launchAtLogin",
        "status.terminal",
        "settings.about",
        "star.open",
    ];
    if cfg!(any(target_os = "macos", windows)) && target.choice == Choice::Podbag {
        keys.insert(1, "settings.vm")
    }
    let i = ui.menu("nav.settings", &keys)?;
    match keys[i] {
        "settings.engine" => {
            if ui.menu("settings.engine", &["engine.connect", "action.inspect"])? == 0 {
                return Ok(Change::Engine(resources::engine(
                    ui,
                    "settings.engine",
                    None,
                )?));
            }
            let c = Client::new(target.endpoint.clone());
            let info = ui.work(&ui.t("settings.engine"), |_| {
                c.version().map_err(|e| e.message)
            })?;
            ui.message(&target.endpoint.describe(), &info.to_string())?;
        }
        "settings.vm" => return vm(ui, exe),
        "settings.updates" => return updates(ui, exe),
        "settings.language" => preferences::appearance(ui, true)?,
        "settings.theme" => preferences::appearance(ui, false)?,
        "settings.launchAtLogin" => preferences::startup(ui, exe)?,
        "status.terminal" => return Ok(Change::Terminal),
        "star.open" => {
            open::that("https://github.com/SteelCrab/podbag").map_err(|e| e.to_string())?
        }
        _ => ui.message(
            &format!("podbag {}", env!("CARGO_PKG_VERSION")),
            &ui.t("settings.aboutText"),
        )?,
    }
    Ok(Change::None)
}

fn updates(ui: &mut Screen, exe: &Path) -> Result<Change> {
    let file = crate::preferences::file("update.json")?;
    let read = |check| cli::updates(exe, Some(&file), check, super::now() as u64);
    let mut versions = ui.work(&ui.t("settings.updates"), |_| Ok(read(cli::Check::Never)))?;
    loop {
        let mut keys = vec!["action.inspect", "update.check"];
        if versions.newer {
            keys.push("update.install")
        }
        let i = ui.menu("settings.updates", &keys)?;
        match keys[i] {
            "update.check" => {
                versions = ui.work(&ui.t("update.check"), |_| Ok(read(cli::Check::Now)))?;
                if let Some(error) = versions.error.as_ref() {
                    return Err(Error::Failed(error.clone()));
                }
                ui.message(
                    &ui.t("settings.updates"),
                    &if versions.newer {
                        ui.t("update.available")
                            .replace("{version}", versions.latest.as_deref().unwrap_or(""))
                    } else {
                        ui.t("update.upToDate")
                    },
                )?;
            }
            "update.install" => {
                ui.confirm(
                    &format!("podbag → {}", versions.latest.as_deref().unwrap_or("")),
                    "update.install",
                )?;
                let result = ui.work(&ui.t("update.installing"), |_| Ok(cli::upgrade(exe)))?;
                let details = [Some(result.output.as_str()), result.command.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.message_with_copy(
                    &ui.t(if result.ok {
                        "update.done"
                    } else {
                        "update.failed"
                    }),
                    &details,
                    result.command.as_deref().unwrap_or(&details),
                )?;
                #[cfg(unix)]
                if result.needs_admin {
                    ui.confirm(&ui.t("update.needsAdmin"), "update.openTerminal")?;
                    return Ok(Change::UpgradeTerminal);
                }
                return Ok(Change::None);
            }
            _ => ui.message(
                &ui.t("settings.updates"),
                &serde_json::to_string_pretty(&versions).map_err(|e| e.to_string())?,
            )?,
        }
    }
}

fn vm(ui: &mut Screen, exe: &Path) -> Result<Change> {
    let state = ui.work(&ui.t("settings.vm"), |_| cli::vm_status(exe))?;
    let running = state["state"] == "running";
    let mut keys = vec!["action.inspect"];
    match state["state"].as_str() {
        Some("running") => keys.extend(["action.stopVm", "action.restartVm"]),
        Some("starting" | "standby") => keys.push("action.stopVm"),
        _ => keys.push("action.startVm"),
    }
    if state["resourcesEditable"] == true {
        keys.push("settings.apply")
    }
    let i = ui.menu("settings.vm", &keys)?;
    let action = match keys[i] {
        "action.inspect" => {
            ui.message(
                &ui.t("settings.vm"),
                &serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?,
            )?;
            return Ok(Change::None);
        }
        "settings.apply" => {
            let next = if state["next"].is_object() {
                &state["next"]
            } else {
                &state["inUse"]
            };
            let mut number = |label: &str, field: &str, unit: &str| -> Result<u64> {
                let title = format!("{} ({unit})", ui.t(label));
                ui.input(
                    &title,
                    &next[field]
                        .as_u64()
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    false,
                    false,
                )?
                .trim()
                .parse()
                .map_err(|_| Error::Failed(format!("Invalid value for {title}")))
            };
            let cpus = number("settings.cpus", "cpus", "vCPU")?
                .try_into()
                .map_err(|_| Error::Failed("Invalid CPU count".into()))?;
            let memory_mib = number("vm.memory", "memoryMiB", "MiB")?;
            let idle_seconds = number("vm.idleSeconds", "idleSeconds", "s")?;
            let restart = running
                && ui.menu(
                    "settings.vmApplyTitle",
                    &["settings.nextStart", "settings.restartNow"],
                )? == 1;
            let values = cli::VmSettings {
                cpus: Some(cpus),
                memory_mib: Some(memory_mib),
                idle_seconds: Some(idle_seconds),
                restart,
            };
            ui.confirm(
                &format!(
                    "{cpus} vCPU · {memory_mib} MiB · {idle_seconds}s · {}",
                    ui.t(if restart {
                        "settings.restartNow"
                    } else {
                        "settings.nextStart"
                    })
                ),
                "settings.apply",
            )?;
            let result = ui.work(&ui.t("settings.apply"), |_| cli::vm_set(exe, &values))?;
            ui.message(&ui.t("settings.vm"), &result)?;
            return Ok(if restart {
                Change::Vm(true)
            } else {
                Change::None
            });
        }
        "action.startVm" => "start",
        "action.stopVm" => "stop",
        _ => "restart",
    };
    if action != "start" {
        let n = state["containers"].as_u64().unwrap_or(0);
        ui.confirm(
            &ui.t(if action == "stop" {
                "confirm.stopVm"
            } else {
                "confirm.restartVm"
            })
            .replace("{n}", &n.to_string()),
            keys[i],
        )?;
    }
    ui.work(&ui.t(keys[i]), |_| cli::vm_action(exe, action))?;
    Ok(Change::Vm(action != "stop"))
}
