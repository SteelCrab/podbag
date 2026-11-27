//! The training panel: the module's steps in a practice session, and what the learner
//! presses now, in the TUI's words and language.

use super::app::{Effect, Lists, Verb};
use super::preferences::Appearance;
use podbag::client::Kind;
use podbag::volume_lesson::course::{self, Attempt, Check, OPEN_WEB, StepDef};
use std::io;
use std::path::Path;

/// The steps of the course attempt in a practice session, as the TUI shows them.
pub struct Panel {
    /// The module's title (the session's `TITLE`), else the module id.
    pub title: String,
    pub steps: Vec<StepDef>,
    pub current: usize,
    pub port: u16,
}

impl Panel {
    /// The attempt in the practice root ROOT.
    #[cfg_attr(not(unix), allow(dead_code))] // Training mode is unix only.
    pub fn load(root: &Path, title: Option<&str>) -> io::Result<Panel> {
        let attempt = Attempt::read(root)?;
        Ok(Panel {
            title: title.unwrap_or(attempt.module()).to_owned(),
            steps: attempt.steps().to_vec(),
            current: attempt.index(),
            port: attempt.port,
        })
    }

    fn step(&self) -> Option<&StepDef> {
        self.steps.get(self.current)
    }

    /// The current step's UI form ("stop", "web"), None for a CLI or finish step.
    pub fn expected(&self) -> Option<(&'static str, String)> {
        self.step()?.ui_form(self.port)
    }

    /// The course engine's rule for ACTION on NAME at the current step: Err with its words
    /// ("This step is remove web.") when they are not the step's UI form.
    pub fn expects(&self, action: &str, name: &str) -> io::Result<()> {
        let step = self.step().ok_or_else(|| io::Error::other("No step."))?;
        course::expects(step, self.port, action, name)
    }

    /// The current step is finish.
    pub fn finished(&self) -> bool {
        self.step().is_some_and(StepDef::is_finish)
    }

    /// The name of step INDEX in LANG, else in English, else its key.
    pub fn name(&self, index: usize, lang: &str) -> String {
        let Some(step) = self.steps.get(index) else {
            return String::new();
        };
        step.names
            .get(lang)
            .or_else(|| step.names.get("en"))
            .unwrap_or(&step.key)
            .clone()
    }
}

/// The UI action an effect does, as the course names it: Act on a container
/// (start/stop/restart/pause; Unpause → "resume"; Remove → "remove"), Act Remove on an image,
/// volume or network ("remove-image", "remove-volume", "remove-network"), Inspect → "inspect",
/// Logs → "logs", Pull(image) → "pull", Prune(kind) → ("prune", kind.segment()), Report → itself.
/// CreateVolume → None: the name is known only after the dialog, so `open_dashboard` then
/// reports `Effect::Report { action: "create-volume", name }`, which goes through this same rule.
pub fn ui_report(effect: &Effect) -> Option<(&'static str, String)> {
    Some(match effect {
        Effect::Act {
            kind, name, verb, ..
        } => {
            let action = match (kind, verb) {
                (Kind::Containers, Verb::Unpause) => "resume",
                (Kind::Containers, verb) => verb.action(),
                (Kind::Images, Verb::Remove { .. }) => "remove-image",
                (Kind::Volumes, Verb::Remove { .. }) => "remove-volume",
                (Kind::Networks, Verb::Remove { .. }) => "remove-network",
                _ => return None,
            };
            (action, name.clone())
        }
        Effect::Inspect { name, .. } => ("inspect", name.clone()),
        Effect::Logs { name, .. } => ("logs", name.clone()),
        Effect::Pull(image) => ("pull", image.clone()),
        Effect::Prune(kind) => ("prune", kind.segment().to_owned()),
        Effect::Report { action, name } => (*action, name.clone()),
        _ => return None,
    })
}

/// What the training shell says first: how to go back, and the current step's whole
/// command (a long one cannot fit the panel). Finish has none: its `exit` ends only the shell.
#[cfg_attr(not(unix), allow(dead_code))] // The training shell is unix only.
pub fn shell_intro(look: &Appearance, panel: &Panel) -> String {
    let mut text = look.text("training.shell");
    text.push('\n');
    if let Some(step) = panel.step().filter(|step| !step.is_finish())
        && let Some(command) = step.command(panel.port)
    {
        let name = panel.name(panel.current, look.lang());
        text.push_str(&format!("\n{name}\n  {command}\n"));
    }
    text
}

/// What the learner does now, in the TUI's words and language.
pub fn hint(look: &Appearance, panel: &Panel, lists: &Lists) -> String {
    let Some(step) = panel.step() else {
        return String::new();
    };
    if step.is_finish() {
        return look.text("training.finished");
    }
    let Some((action, name)) = panel.expected() else {
        return step
            .command(panel.port)
            .map(|command| look.text("training.cli").replace("{command}", &command))
            .unwrap_or_default();
    };
    if action == OPEN_WEB {
        return look
            .text("training.do.open-web")
            .replace("{url}", &format!("http://127.0.0.1:{}/", panel.port));
    }
    let (key, tab) = match action {
        "pull" => ("pull", Kind::Images),
        "remove-image" => ("remove", Kind::Images),
        "create-volume" => ("create-volume", Kind::Volumes),
        "remove-volume" => ("remove", Kind::Volumes),
        "remove-network" => ("remove", Kind::Networks),
        "prune" => (
            "prune",
            Kind::ALL
                .into_iter()
                .find(|kind| kind.segment() == name)
                .unwrap_or(Kind::Containers),
        ),
        "inspect" => ("inspect", inspected_in(&step.check, lists, &name)),
        // start, stop, restart, pause, resume, remove, logs: a container.
        other => (other, Kind::Containers),
    };
    look.text(&format!("training.do.{key}"))
        .replace("{tab}", &look.text(&format!("nav.{}", tab.segment())))
        .replace("{name}", &name)
}

/// The list of an inspect step's object: the one its desktop-* check names, else the
/// first list that has NAME.
fn inspected_in(check: &Check, lists: &Lists, name: &str) -> Kind {
    match check {
        Check::DesktopInspectVolume { .. } => Kind::Volumes,
        Check::DesktopInspectExited { .. } | Check::DesktopPorts { .. } => Kind::Containers,
        _ => listed_in(lists, name),
    }
}

/// The first list that has NAME (containers, volumes, networks), else Containers.
fn listed_in(lists: &Lists, name: &str) -> Kind {
    if lists.containers.iter().any(|c| c.name() == name) {
        Kind::Containers
    } else if lists.volumes.iter().any(|v| v.name == name) {
        Kind::Volumes
    } else if lists.networks.iter().any(|n| n.name == name) {
        Kind::Networks
    } else {
        Kind::Containers
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::app::App;
    use super::super::view::{buffer_text, draw};
    use super::*;
    use podbag::volume_lesson::course::{Check, Ui, UiAction};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn step(key: &str, name: &str, command: Option<&str>, check: Check, ui: Option<Ui>) -> StepDef {
        StepDef {
            key: key.into(),
            names: BTreeMap::from([("en".to_string(), name.to_string())]),
            command: command.map(str::to_owned),
            check,
            ui,
        }
    }

    fn ui_step_named(title: &str, action: &str, name: &str, check: Check) -> StepDef {
        let ui = Ui {
            action: UiAction::parse(action).unwrap(),
            name: name.into(),
        };
        step(action, title, None, check, Some(ui))
    }

    fn ui_step(action: &str, name: &str, check: Check) -> StepDef {
        ui_step_named(action, action, name, check)
    }

    fn cli_step(command: &str) -> StepDef {
        step(
            "cli",
            command,
            Some(command),
            Check::ContainerRunning { name: "web".into() },
            None,
        )
    }

    fn cli_step_named(title: &str) -> StepDef {
        step(
            "run",
            title,
            Some("podbag run -d --name web nginx"),
            Check::ContainerRunning { name: "web".into() },
            None,
        )
    }

    fn browser_step(name: &str) -> StepDef {
        step(
            "browser",
            "Open the page",
            None,
            Check::DesktopBrowser {
                name: name.into(),
                page: "Hello".into(),
            },
            None,
        )
    }

    fn finish_step() -> StepDef {
        step("finish", "Finish", None, Check::Finish, None)
    }

    fn desktop_stop(name: &str) -> Check {
        Check::DesktopStop { name: name.into() }
    }

    fn image_exists(name: &str) -> Check {
        Check::ImageExists { name: name.into() }
    }

    fn volume_absent(name: &str) -> Check {
        Check::VolumeAbsent { name: name.into() }
    }

    fn panel(steps: Vec<StepDef>, current: usize) -> Panel {
        Panel {
            title: "Open a web server".into(),
            steps,
            current,
            port: 18080,
        }
    }

    fn look(lang: &str) -> Appearance {
        Appearance {
            language: lang.into(),
            ..Default::default()
        }
    }

    fn draw_text(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        buffer_text(t.backend().buffer())
    }

    #[test]
    fn hints_name_the_key_and_the_object() {
        let en = look("en");
        let lists = Lists::default();
        assert_eq!(
            hint(
                &en,
                &panel(vec![ui_step("stop", "web", desktop_stop("web"))], 0),
                &lists
            ),
            "Select web in Containers and press x"
        );
        assert_eq!(
            hint(
                &en,
                &panel(
                    vec![ui_step(
                        "pull",
                        "busybox:1.37.0",
                        image_exists("busybox:1.37.0")
                    )],
                    0
                ),
                &lists
            ),
            "Open Images, press p, type busybox:1.37.0 and press Enter"
        );
        assert_eq!(
            hint(
                &en,
                &panel(
                    vec![ui_step("remove-volume", "data", volume_absent("data"))],
                    0
                ),
                &lists
            ),
            "Select data in Volumes, press d, then y"
        );
        assert_eq!(
            hint(&en, &panel(vec![cli_step("podbag ps -a")], 0), &lists),
            "Press : and run: podbag ps -a"
        );
        assert_eq!(
            hint(&en, &panel(vec![browser_step("web")], 0), &lists),
            "Open http://127.0.0.1:18080/ in a browser. When the page shows, press o"
        );
        assert_eq!(
            hint(&en, &panel(vec![finish_step()], 0), &lists),
            "All steps are done. Press q to end the session."
        );
        let ko = hint(
            &look("ko"),
            &panel(vec![ui_step("stop", "web", desktop_stop("web"))], 0),
            &lists,
        );
        assert!(
            ko.contains("web") && ko.contains("x") && ko.contains("컨테이너"),
            "{ko}"
        );
    }

    #[test]
    fn hints_find_the_tab_of_the_object() {
        let en = look("en");
        let mut lists = Lists::default();
        // A step whose check names no resource type: the first list that has the name.
        let inspect = panel(
            vec![ui_step(
                "inspect",
                "data",
                Check::OutputContains {
                    text: "local".into(),
                },
            )],
            0,
        );
        // Not listed: Containers.
        assert_eq!(
            hint(&en, &inspect, &lists),
            "Select data in Containers and press Enter"
        );
        lists.volumes.push(podbag::client::VolumeSummary {
            name: "data".into(),
            ..Default::default()
        });
        assert_eq!(
            hint(&en, &inspect, &lists),
            "Select data in Volumes and press Enter"
        );
        // desktop-inspect-volume names a volume: Volumes, also when a container has the name.
        lists.containers.push(podbag::client::ContainerSummary {
            names: vec!["/data".into()],
            ..Default::default()
        });
        let volume = panel(
            vec![step(
                "inspectVolume",
                "Open the volume",
                None,
                Check::DesktopInspectVolume {
                    name: "data".into(),
                },
                None,
            )],
            0,
        );
        assert_eq!(
            hint(&en, &volume, &lists),
            "Select data in Volumes and press Enter"
        );
        assert_eq!(
            hint(&en, &volume, &Lists::default()),
            "Select data in Volumes and press Enter"
        );
        let prune = panel(vec![ui_step("prune", "images", image_exists("busybox"))], 0);
        assert_eq!(hint(&en, &prune, &lists), "Open Images, press c, then y");
        let network = panel(
            vec![ui_step(
                "remove-network",
                "lab",
                Check::NetworkAbsent { name: "lab".into() },
            )],
            0,
        );
        assert_eq!(
            hint(&en, &network, &lists),
            "Select lab in Networks, press d, then y"
        );
    }

    #[test]
    fn the_panel_knows_the_step() {
        let p = panel(
            vec![
                cli_step_named("Run the server"),
                ui_step_named("Stop web", "stop", "web", desktop_stop("web")),
                finish_step(),
            ],
            1,
        );
        assert_eq!(p.expected(), Some(("stop", "web".to_string())));
        assert!(!p.finished());
        assert_eq!(p.name(0, "ko"), "Run the server");
        let done = Panel { current: 2, ..p };
        assert_eq!(done.expected(), None);
        assert!(done.finished());
    }

    #[test]
    fn the_panel_shows_done_current_and_next() {
        let mut app = App::new("podbag".into(), "test".into());
        app.appearance.language = "en".into();
        app.training = Some(panel(
            vec![
                cli_step_named("Run the server"),
                ui_step_named("Stop web", "stop", "web", desktop_stop("web")),
                finish_step(),
            ],
            1,
        ));
        let s = draw_text(&mut app, 80, 24);
        assert!(s.contains("Training · Open a web server"), "{s}");
        assert!(
            s.contains("✓ Run the server") && s.contains("Stop web") && s.contains("press x"),
            "{s}"
        );
        assert!(s.contains("▶ 2/3 Stop web") && s.contains("Finish"), "{s}");
        // The key line is still drawn.
        assert!(s.lines().last().unwrap().ends_with("q quit"), "{s}");
    }

    #[test]
    fn a_panel_loads_from_the_practice_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("run")).unwrap();
        let course = podbag::volume_lesson::course::Course {
            id: "web-basics".into(),
            port: None,
            steps: vec![cli_step_named("Run the server"), finish_step()],
            files: BTreeMap::new(),
        };
        podbag::volume_lesson::course::Attempt::start(dir.path(), &course).unwrap();
        let p = Panel::load(dir.path(), None).unwrap();
        assert_eq!(
            (p.title.as_str(), p.current, p.steps.len()),
            ("web-basics", 0, 2)
        );
        let p = Panel::load(dir.path(), Some("Web basics")).unwrap();
        assert_eq!(p.title, "Web basics");
    }

    /// The panel's text at W×H, without its borders and spaces: wrapped words join up.
    fn panel_text(app: &mut App, w: u16, h: u16) -> (String, usize) {
        let s = draw_text(app, w, h);
        let lines: Vec<&str> = s.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.starts_with("┌ Training"))
            .expect(&s);
        let text = lines[top + 1..]
            .iter()
            .take_while(|l| !l.starts_with('└'))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect();
        (text, top)
    }

    fn squash(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// An app at the first of STEPS, in LANG.
    fn training_app(lang: &str, steps: Vec<StepDef>) -> App {
        let mut app = App::new("podbag".into(), "test".into());
        app.appearance.language = lang.into();
        app.training = Some(panel(steps, 0));
        app
    }

    /// The whole hint is drawn at 80×24 (the key is its last word), and the lists keep
    /// 3 rows: the header and tabs take rows 0 and 1, so the panel starts at row 5 or later.
    fn assert_whole_hint(lang: &str, steps: Vec<StepDef>) {
        let mut app = training_app(lang, steps);
        let want = hint(
            &app.appearance,
            app.training.as_ref().unwrap(),
            &Lists::default(),
        );
        let (text, top) = panel_text(&mut app, 80, 24);
        assert!(
            text.contains(&squash(&want)),
            "{lang}: {want:?} not in {text:?}"
        );
        assert!(top >= 5, "{lang}: the panel starts at row {top}");
    }

    #[test]
    fn the_key_is_drawn_in_every_language() {
        for lang in crate::preferences::LANGS {
            assert_whole_hint(lang, vec![browser_step("web"), finish_step()]);
        }
        let pull = || {
            vec![ui_step(
                "pull",
                "busybox:1.37.0",
                image_exists("busybox:1.37.0"),
            )]
        };
        assert_whole_hint("fil", pull());
        let volume = vec![ui_step(
            "create-volume",
            "web-data",
            Check::VolumeExists {
                name: "web-data".into(),
            },
        )];
        assert_whole_hint("fil", volume);
        for lang in ["ko", "ja"] {
            let long = "docker.io/library/nginx:1.27-alpine";
            assert_whole_hint(lang, vec![ui_step("pull", long, image_exists(long))]);
        }
    }

    #[test]
    fn a_long_command_is_drawn_whole() {
        // As long as the longest command in the shipped modules (305 characters).
        let command = format!(
            "podbag run -d --name lab-web --restart on-failure:3 -p 127.0.0.1:{{port}}:80 {} nginx:1.27-alpine sh -c 'echo ready; exec nginx -g \"daemon off;\"' END",
            "-e GREETING=hello ".repeat(10)
        );
        assert!(command.len() >= 300, "{}", command.len());
        let mut app = training_app("en", vec![cli_step(&command), finish_step()]);
        let (text, top) = panel_text(&mut app, 80, 24);
        let shown = command.replace("{port}", "18080");
        assert!(text.contains(&squash(&shown)), "{text}");
        assert!(text.ends_with(&squash("END ○ Finish")), "{text}");
        assert!(top >= 5, "the panel starts at row {top}");
    }

    #[test]
    fn the_key_line_keeps_help_and_quit_in_every_language() {
        for lang in crate::preferences::LANGS {
            let mut app = training_app(lang, vec![browser_step("web"), finish_step()]);
            let s = draw_text(&mut app, 80, 24);
            let last = s.lines().last().unwrap();
            let look = &app.appearance;
            let tail = format!("? {} q {}", look.hint("help"), look.hint("quit"));
            // Whole, at the end of 80 columns: the line fits.
            assert!(
                squash(last).ends_with(&squash(&tail)),
                "{lang}: {tail:?} not at the end of {last:?}"
            );
            assert!(
                app.areas.keys.iter().all(|(r, _)| r.right() <= 80),
                "{lang}"
            );
        }
        // In English the end of the middle goes: `/ find`, then `e shell`.
        let mut app = training_app("en", vec![browser_step("web"), finish_step()]);
        let s = draw_text(&mut app, 80, 24);
        assert_eq!(
            s.lines().last().unwrap(),
            "a actions : training s start x stop r restart d remove l logs ? help q quit"
        );
    }

    #[test]
    fn effects_report_as_the_course_names_them() {
        use super::super::app::{Effect, Verb};
        let act = |kind, verb| Effect::Act {
            kind,
            id: "i".into(),
            name: "web".into(),
            verb,
        };
        assert_eq!(
            ui_report(&act(Kind::Containers, Verb::Stop)),
            Some(("stop", "web".into()))
        );
        assert_eq!(
            ui_report(&act(Kind::Containers, Verb::Unpause)),
            Some(("resume", "web".into()))
        );
        assert_eq!(
            ui_report(&act(Kind::Containers, Verb::Remove { force: true })),
            Some(("remove", "web".into()))
        );
        assert_eq!(
            ui_report(&act(Kind::Images, Verb::Remove { force: false })),
            Some(("remove-image", "web".into()))
        );
        assert_eq!(
            ui_report(&act(Kind::Volumes, Verb::Remove { force: false })),
            Some(("remove-volume", "web".into()))
        );
        assert_eq!(
            ui_report(&act(Kind::Networks, Verb::Remove { force: false })),
            Some(("remove-network", "web".into()))
        );
        assert_eq!(
            ui_report(&Effect::Inspect {
                kind: Kind::Volumes,
                id: "data".into(),
                name: "data".into(),
                running: false
            }),
            Some(("inspect", "data".into()))
        );
        assert_eq!(
            ui_report(&Effect::Logs {
                id: "i".into(),
                name: "web".into()
            }),
            Some(("logs", "web".into()))
        );
        assert_eq!(
            ui_report(&Effect::Pull("busybox:1.37.0".into())),
            Some(("pull", "busybox:1.37.0".into()))
        );
        assert_eq!(
            ui_report(&Effect::Prune(Kind::Volumes)),
            Some(("prune", "volumes".into()))
        );
        // CreateVolume has no name yet; open_dashboard reports it as Effect::Report afterwards.
        assert_eq!(ui_report(&Effect::CreateVolume), None);
        assert_eq!(
            ui_report(&Effect::Report {
                action: "create-volume",
                name: "data".into()
            }),
            Some(("create-volume", "data".into()))
        );
        assert_eq!(ui_report(&Effect::Refresh), None);
        assert_eq!(
            ui_report(&Effect::Rename {
                id: "i".into(),
                to: "web2".into()
            }),
            None
        );
    }

    #[test]
    fn the_shell_shows_the_steps_whole_command() {
        let p = panel(
            vec![
                cli_step_named("Run the server"),
                ui_step_named("Stop web", "stop", "web", desktop_stop("web")),
                finish_step(),
            ],
            0,
        );
        assert_eq!(
            shell_intro(&look("en"), &p),
            "Training shell. Type exit to return to the TUI.\n\nRun the server\n  podbag run -d --name web nginx\n"
        );
        // A step without a command, and finish (its command `exit` ends only this shell).
        for current in [1, 2] {
            let p = Panel {
                current,
                ..panel(p.steps.clone(), 0)
            };
            assert_eq!(
                shell_intro(&look("en"), &p),
                "Training shell. Type exit to return to the TUI.\n"
            );
        }
    }

    #[test]
    fn a_huge_hint_leaves_the_lists_three_rows() {
        let command = "podbag run x ".repeat(200);
        let mut app = training_app("en", vec![cli_step(&command), finish_step()]);
        let s = draw_text(&mut app, 80, 24);
        let top = s.lines().position(|l| l.starts_with("┌ Training")).unwrap();
        assert_eq!(top, 5, "{s}");
        assert!(s.lines().last().unwrap().ends_with("q quit"), "{s}");
    }
}
