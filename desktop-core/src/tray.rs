//! What the tray (menu bar) item shows: the engine's state and the texts of its menu.
//! The UI toolkit draws the menu; this decides what it says.

use crate::dockercmd::DockerCommand;
use crate::engine::EngineStatus;
use crate::strings;

/// What the tray shows of the engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EngineState {
    #[default]
    Unknown,
    Ready,
    Down,
    VmRunning,
    VmStopped,
}

impl EngineState {
    /// The state an engine check found.
    pub fn of(status: &EngineStatus) -> EngineState {
        match (status.vm.as_deref(), status.reachable) {
            (Some("running"), _) => EngineState::VmRunning,
            (Some(_), false) => EngineState::VmStopped,
            (_, true) => EngineState::Ready,
            (_, false) => EngineState::Down,
        }
    }
}

/// What the tray's inputs are, as the app last saw them.
#[derive(Debug, Clone, Default)]
pub struct TrayInput {
    pub language: &'static str,
    pub engine: EngineState,
    /// Running containers, as last seen.
    pub running: Option<usize>,
    /// The engine's name for the user (`podbag`, `Docker`, …).
    pub engine_name: String,
    pub uses_podbag: bool,
    /// A newer release.
    pub update: Option<String>,
    pub docker: Option<DockerCommand>,
}

/// The `docker` submenu when the docker CLI is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSwitch {
    /// `docker` goes to podbag: "podbag" is checked and the other item switches back.
    pub is_podbag: bool,
    /// The other item: where `docker` goes now, or where it goes back to.
    pub other: String,
}

/// The texts and states of the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayModel {
    pub engine: String,
    /// "Update available (…)": shown only while one is known.
    pub update: Option<String>,
    pub status: String,
    pub running: String,
    pub open: String,
    pub settings: String,
    /// The `docker` submenu's title.
    pub docker: String,
    /// None: the docker CLI is not installed, and the submenu is disabled.
    pub docker_switch: Option<DockerSwitch>,
    pub quit: String,
    /// Start or stop the VM (macOS).
    pub vm: String,
    pub vm_enabled: bool,
}

impl TrayModel {
    pub fn new(input: &TrayInput) -> TrayModel {
        let lang = input.language;
        let t = |key: &str| strings::text(lang, key, None);
        let engine = input.engine;
        let status = match engine {
            EngineState::Unknown => t("tray.checking"),
            EngineState::Ready => t("tray.engineRunning"),
            EngineState::Down => t("tray.engineDown"),
            EngineState::VmRunning => t("tray.vmRunning"),
            EngineState::VmStopped => t("tray.vmStopped"),
        };
        let running = match (engine, input.running) {
            (EngineState::VmStopped, _) => strings::text(lang, "tray.running", Some(0)),
            (_, Some(n)) => strings::text(lang, "tray.running", Some(n)),
            (_, None) => t("tray.runningUnknown"),
        };
        let (docker, docker_switch) = match &input.docker {
            Some(d) if d.installed => {
                let other = if d.is_podbag {
                    let back = d.previous.clone().unwrap_or_else(|| "default".into());
                    strings::fill(lang, "tray.dockerBack", &[("name", &back)])
                } else {
                    d.name.clone()
                };
                (
                    strings::fill(lang, "tray.docker", &[("name", &d.name)]),
                    Some(DockerSwitch {
                        is_podbag: d.is_podbag,
                        other,
                    }),
                )
            }
            _ => (t("tray.dockerMissing"), None),
        };
        let vm = if engine == EngineState::VmRunning {
            "tray.stopVm"
        } else {
            "tray.startVm"
        };
        TrayModel {
            engine: strings::fill(lang, "tray.engine", &[("name", &input.engine_name)]),
            update: input
                .update
                .as_ref()
                .map(|v| strings::fill(lang, "tray.update", &[("version", v)])),
            status,
            running,
            open: t("tray.open"),
            settings: t("tray.settings"),
            docker,
            docker_switch,
            quit: t("tray.quit"),
            vm: t(vm),
            vm_enabled: engine != EngineState::Unknown && input.uses_podbag,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> TrayInput {
        TrayInput {
            language: "en",
            engine_name: "podbag".into(),
            uses_podbag: true,
            ..TrayInput::default()
        }
    }

    #[test]
    fn engine_checks_set_the_state() {
        let status = |vm: Option<&str>, reachable| EngineStatus {
            vm: vm.map(str::to_string),
            reachable,
            ..EngineStatus::default()
        };
        assert_eq!(
            EngineState::of(&status(Some("running"), false)),
            EngineState::VmRunning
        );
        assert_eq!(
            EngineState::of(&status(Some("stopped"), false)),
            EngineState::VmStopped
        );
        assert_eq!(
            EngineState::of(&status(Some("stopped"), true)),
            EngineState::Ready
        );
        assert_eq!(EngineState::of(&status(None, true)), EngineState::Ready);
        assert_eq!(EngineState::of(&status(None, false)), EngineState::Down);
    }

    #[test]
    fn an_unknown_engine_disables_the_vm_item() {
        let m = TrayModel::new(&input());
        assert_eq!(m.engine, "Engine: podbag");
        assert_eq!(m.status, strings::text("en", "tray.checking", None));
        assert_eq!(m.running, strings::text("en", "tray.runningUnknown", None));
        assert_eq!(m.update, None);
        assert!(!m.vm_enabled);
        assert_eq!(m.docker_switch, None);
        assert_eq!(m.docker, strings::text("en", "tray.dockerMissing", None));
    }

    #[test]
    fn a_stopped_vm_runs_no_containers() {
        let m = TrayModel::new(&TrayInput {
            engine: EngineState::VmStopped,
            running: Some(4),
            update: Some("0.4.0".into()),
            ..input()
        });
        assert_eq!(m.running, "Running: 0");
        assert_eq!(m.update.as_deref(), Some("Update available (0.4.0)…"));
        assert!(m.vm_enabled);
        assert_eq!(m.vm, strings::text("en", "tray.startVm", None));
        let running = TrayModel::new(&TrayInput {
            engine: EngineState::VmRunning,
            running: Some(4),
            ..input()
        });
        assert_eq!(running.running, "Running: 4");
        assert_eq!(running.vm, strings::text("en", "tray.stopVm", None));
        let docker = TrayModel::new(&TrayInput {
            engine: EngineState::Ready,
            uses_podbag: false,
            ..input()
        });
        assert!(!docker.vm_enabled, "VM controls need podbag");
    }

    #[test]
    fn the_docker_menu_offers_the_other_place() {
        let at = |is_podbag: bool, previous: Option<&str>| {
            TrayModel::new(&TrayInput {
                docker: Some(DockerCommand {
                    installed: true,
                    name: if is_podbag {
                        "podbag"
                    } else {
                        "Docker Desktop"
                    }
                    .into(),
                    is_podbag,
                    previous: previous.map(str::to_string),
                    ..DockerCommand::default()
                }),
                ..input()
            })
        };
        let desktop = at(false, None);
        assert_eq!(
            desktop.docker_switch,
            Some(DockerSwitch {
                is_podbag: false,
                other: "Docker Desktop".into()
            })
        );
        let podbag = at(true, Some("desktop-linux"));
        let switch = podbag.docker_switch.unwrap();
        assert!(switch.is_podbag);
        assert_eq!(
            switch.other,
            strings::fill("en", "tray.dockerBack", &[("name", "desktop-linux")])
        );
        let other = at(true, None).docker_switch.unwrap().other;
        assert_eq!(
            other,
            strings::fill("en", "tray.dockerBack", &[("name", "default")])
        );
    }
}
