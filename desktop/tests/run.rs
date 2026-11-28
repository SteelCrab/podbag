//! The run form on the mock backend: from the Containers toolbar, from an image's row
//! and from the empty Containers page. Bad input is named; good input goes to the engine
//! as it is; a missing image is pulled first; the new container's logs open.

use std::sync::mpsc::{Receiver, Sender, channel};

use podbag_desktop::app::Tab;
use podbag_desktop::dialogs::{self, Dialog};
use podbag_desktop::views::resources;
use podbag_desktop::views::run::{self, RunForm};
use podbag_desktop::{Desktop, Msg, Options, Page};
use podbag_desktop_core::mock::{Calls, Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, Event, Kind, PublishedPort, RunSpec, VolumeMount};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, NamedKey, Synthetic};

type H = Harness<UiView<Desktop>>;

/// The window, with the backend's events held until `wake` (or `step`) passes them on.
struct Window {
    h: H,
    from_backend: Receiver<Event>,
    to_window: Sender<Event>,
    calls: Calls,
}

impl Window {
    fn new(scenario: Scenario, page: Page) -> Window {
        podbag_desktop::icons::install();
        let calls = Calls::default();
        let opts = MockOptions {
            scenario,
            instant: true,
            all_features: true,
            calls: calls.clone(),
            ..MockOptions::default()
        };
        let (backend, from_backend) = Backend::mock(opts, || {});
        let (to_window, events) = channel();
        let app = Desktop::new(backend, events, Options::inline());
        let mut w = Window {
            h: Harness::new(UiView::new(app), 1200., 800.),
            from_backend,
            to_window,
            calls,
        };
        w.wake();
        w.h.view_mut().app.go(page);
        w.wake();
        w
    }

    fn app(&self) -> &Desktop {
        &self.h.view().app
    }

    /// Pass on what the backend reported, and paint.
    fn wake(&mut self) {
        for e in self.from_backend.try_iter() {
            self.to_window.send(e).unwrap();
        }
        self.h.message(Msg::Wake);
        self.h.repaint();
    }

    /// Pass on the backend's events up to the first that `stop` picks, and paint.
    fn step(&mut self, stop: impl Fn(&Event) -> bool) {
        while let Ok(e) = self.from_backend.try_recv() {
            let last = stop(&e);
            self.to_window.send(e).unwrap();
            if last {
                break;
            }
        }
        self.h.message(Msg::Wake);
        self.h.repaint();
    }

    fn form(&self) -> &RunForm {
        match &self.app().dialog {
            Some(Dialog::Run(f)) => f,
            _ => panic!("the run form is not open"),
        }
    }

    fn form_mut(&mut self) -> &mut RunForm {
        match &mut self.h.view_mut().app.dialog {
            Some(Dialog::Run(f)) => f,
            _ => panic!("the run form is not open"),
        }
    }

    fn open(&self) -> bool {
        matches!(self.app().dialog, Some(Dialog::Run(_)))
    }

    fn click(&mut self, id: Id) {
        self.h.click_id(id);
    }

    /// Replace the text of the field `id`.
    fn set(&mut self, id: Id, text: &str) {
        self.h.click_id(id);
        let m = if cfg!(target_os = "macos") {
            ModifiersState::SUPER
        } else {
            ModifiersState::CONTROL
        };
        self.h.press_key(KeyPress::char("a").with_mods(m));
        if text.is_empty() {
            self.h.press_key(KeyPress::named(NamedKey::Backspace));
        } else {
            self.h.input(Synthetic::Text(text.into()));
        }
    }

    /// Choose item `n` of the select `id`.
    fn choose(&mut self, id: Id, n: usize) {
        self.h.click_id(id);
        self.h.click_id(id.with(("item", n)));
    }

    fn submit(&mut self) {
        self.click(run::RUN);
        self.wake();
    }

    fn status(&self) -> String {
        self.form().status.clone()
    }

    fn sent(&self) -> Vec<RunSpec> {
        self.calls
            .of("run_container")
            .iter()
            .map(|json| serde_json::from_str(json).unwrap())
            .collect()
    }

    fn toasted(&self, part: &str) -> bool {
        self.app().toasts.texts().any(|(t, _)| t.contains(part))
    }
}

/// The form from the Containers page: bad input is named, good input is sent as it is.
#[test]
fn runs_from_the_form() {
    let mut w = Window::new(Scenario::Default, Page::Containers);
    // Tall enough for the whole form with its options open.
    w.h.resize(1200., 1200.);
    w.click(run::OPEN);
    assert!(
        w.open(),
        "Run container on the Containers page opens the form"
    );
    assert_eq!(w.h.focused(), Some(run::IMAGE), "the image has the focus");
    assert!(!w.form().more, "the options are closed at first");

    w.submit();
    assert_eq!(
        w.status(),
        "Enter an image name without spaces, like nginx:alpine."
    );
    assert_eq!(
        w.form().invalid,
        Some(run::IMAGE),
        "the image field is marked"
    );
    assert!(w.form().error);

    w.set(run::IMAGE, "nginx:alpine");
    assert_eq!(w.form().invalid, None, "typing clears the mark");
    assert!(w.status().is_empty());
    w.set(run::NAME, "web");
    let (host, container) = (w.form().ports[0].host.id, w.form().ports[0].container.id);
    w.set(host, "8080");
    w.set(container, "70000");
    w.submit();
    assert_eq!(w.status(), "A port is a number from 1 to 65535.");
    assert_eq!(w.h.focused(), Some(container), "the bad port has the focus");
    w.set(container, "80");

    // A second port: a UDP one with no host port.
    w.click(run::ADD_PORT);
    assert_eq!(w.form().ports.len(), 2);
    let second = (w.form().ports[1].host.id, w.form().ports[1].container.id);
    assert_eq!(
        w.h.focused(),
        Some(second.0),
        "the new port row has the focus"
    );
    w.set(second.1, "53");
    let protocol = w.form().ports[1].protocol_id();
    w.choose(protocol, 1);
    assert!(w.form().ports[1].udp);

    // A variable without a value, while the options are closed: they open to show it.
    let env = w.form_mut().add_env();
    w.form_mut().env[0].input.set_content("MODE");
    w.submit();
    assert!(w.form().more, "the options open to show the problem");
    assert_eq!(w.status(), "Write each variable as KEY=VALUE.");
    assert_eq!(w.h.focused(), Some(env));
    w.set(env, "MODE=dev");

    w.click(run::ADD_VOLUME);
    let (source, target) = (w.form().volumes[0].source.id, w.form().volumes[0].target.id);
    assert_eq!(w.h.focused(), Some(source));
    w.set(source, "./site");
    w.set(target, "/usr/share/nginx/html");
    w.submit();
    assert!(
        w.status()
            .starts_with("Enter a volume name or a full host path"),
        "a relative path is refused: {}",
        w.status()
    );
    w.set(source, "site");
    let read_only = w.form().volumes[0].read_only_id();
    w.click(read_only);
    assert!(w.form().volumes[0].read_only);
    w.click(run::ADD_VOLUME);
    let logs = (w.form().volumes[1].source.id, w.form().volumes[1].target.id);
    w.set(logs.0, "/Users/me/logs");
    w.set(logs.1, "data");
    w.submit();
    assert_eq!(
        w.status(),
        "Enter a full path in the container, like /data."
    );
    w.set(logs.1, "/var/log/nginx");

    w.set(run::COMMAND, "nginx -g 'daemon off;");
    w.submit();
    assert_eq!(w.status(), "A quote in the command is not closed.");
    w.set(run::COMMAND, "nginx -g 'daemon off;'");
    w.choose(run::RESTART, 2);
    assert_eq!(w.form().restart, 2, "always");
    // Remove when it stops goes only with no restart.
    w.click(run::AUTO_REMOVE);
    assert!(w.form().auto_remove);
    assert_eq!(w.form().restart, 0, "no restart");
    assert!(
        w.h.hitbox(run::RESTART).is_none(),
        "the restart policy is disabled"
    );
    w.click(run::AUTO_REMOVE);
    w.choose(run::RESTART, 3);
    assert!(
        w.sent().is_empty(),
        "nothing is sent while the form has a problem"
    );

    // Enter sends the form from a select too, not only from a text field.
    assert_eq!(w.h.focused(), Some(run::RESTART));
    w.h.press_key(KeyPress::named(NamedKey::Enter));
    w.wake();
    assert!(!w.open(), "the form closes");
    let want = RunSpec {
        image: "nginx:alpine".into(),
        name: "web".into(),
        ports: vec![
            PublishedPort {
                host: 8080,
                container: 80,
                protocol: "tcp".into(),
            },
            PublishedPort {
                host: 0,
                container: 53,
                protocol: "udp".into(),
            },
        ],
        env: vec!["MODE=dev".into()],
        volumes: vec![
            VolumeMount {
                source: "site".into(),
                target: "/usr/share/nginx/html".into(),
                read_only: true,
            },
            VolumeMount {
                source: "/Users/me/logs".into(),
                target: "/var/log/nginx".into(),
                read_only: false,
            },
        ],
        command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
        restart: "unless-stopped".into(),
        auto_remove: false,
    };
    assert_eq!(w.sent(), [want], "the form is sent as it is, once");
    let detail = w.app().detail.as_ref().expect("the new container opens");
    assert_eq!(detail.tab, Tab::Logs, "on its logs");
    let i = w.app().lists.find(Kind::Containers, &detail.id).unwrap();
    assert_eq!(w.app().lists.containers[i].name(), "web");
    assert!(w.toasted("Started web"));
}

/// Run on an image row: the image is filled in; Esc closes; a missing image is pulled.
#[test]
fn runs_an_image_from_its_row() {
    let mut w = Window::new(Scenario::Default, Page::Images);
    let i = w
        .app()
        .lists
        .images
        .iter()
        .position(|i| i.repo_tags.contains(&"alpine:3.22".to_string()))
        .unwrap();
    let run_button = resources::table_id(Kind::Images)
        .with(("row", i))
        .with("run");
    w.click(run_button);
    assert!(w.open(), "each image offers Run");
    assert_eq!(
        w.form().image.content(),
        "alpine:3.22",
        "the image is filled in"
    );
    assert_eq!(
        w.h.focused(),
        Some(run::NAME),
        "the name has the focus when the image is given"
    );
    w.h.press_key(KeyPress::named(NamedKey::Escape));
    assert!(!w.open(), "Esc closes the form");

    w.click(run_button);
    w.set(run::IMAGE, "busybox:1.37");
    w.click(run::MORE);
    w.click(run::AUTO_REMOVE);
    w.click(run::RUN);
    assert!(w.form().running, "the form waits for the engine");
    assert!(
        w.h.hitbox(dialogs::CANCEL).is_none(),
        "nothing can be changed meanwhile"
    );
    w.h.press_key(KeyPress::named(NamedKey::Escape));
    assert!(w.open(), "Esc does not close it meanwhile");
    assert_eq!(w.status(), "Starting busybox:1.37…");
    w.step(|e| matches!(e, Event::PullProgress { .. }));
    assert_eq!(
        w.status(),
        "Pulling busybox:1.37…\nPulling from library/busybox:1.37",
        "the pull shows"
    );
    w.wake();
    assert!(!w.open(), "the form closes after the pull");
    let sent = w.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].image, "busybox:1.37");
    assert!(sent[0].auto_remove && sent[0].restart == "no" && sent[0].name.is_empty());
    let detail = w.app().detail.as_ref().expect("the new container opens");
    assert_eq!(detail.tab, Tab::Logs);
    assert!(w.toasted("Started eager_tesla"));
    assert!(
        w.app()
            .lists
            .images
            .iter()
            .any(|i| i.repo_tags.contains(&"busybox:1.37".to_string())),
        "the image was pulled"
    );
}

/// The image list offers the engine's tags; typing filters it.
#[test]
fn the_image_list_offers_local_tags() {
    let mut w = Window::new(Scenario::Default, Page::Containers);
    w.click(run::OPEN);
    assert!(!w.form().suggest);
    w.click(run::IMAGES);
    assert!(w.form().suggest, "the arrow shows the tags");
    assert_eq!(
        w.h.focused(),
        Some(run::IMAGE),
        "the focus stays in the field"
    );
    assert_eq!(w.form().suggestions().len(), 5);
    w.h.input(Synthetic::Text("red".into()));
    assert_eq!(w.form().suggestions(), ["redis:7-alpine"]);
    w.click(run::suggestion_id(0));
    assert_eq!(w.form().image.content(), "redis:7-alpine");
    assert!(!w.form().suggest);
    // The keyboard: Down shows them, Enter picks one.
    w.set(run::IMAGE, "");
    w.h.press_key(KeyPress::named(NamedKey::Escape));
    assert!(w.open(), "Esc closes the list first");
    w.h.press_key(KeyPress::named(NamedKey::ArrowDown));
    w.h.press_key(KeyPress::named(NamedKey::ArrowDown));
    w.h.press_key(KeyPress::named(NamedKey::Enter));
    assert_eq!(w.form().image.content(), "postgres:16-alpine");
    assert!(
        w.sent().is_empty(),
        "Enter on the list picks, it does not send"
    );
}

/// An engine error shows in the form, which can be sent again.
#[test]
fn an_engine_error_stays_in_the_form() {
    let mut w = Window::new(Scenario::Default, Page::Containers);
    w.click(run::OPEN);
    w.set(run::IMAGE, "notfound/app");
    w.set(run::NAME, "web2");
    w.submit();
    assert!(w.open());
    assert_eq!(w.status(), "notfound/app:latest: manifest unknown");
    assert!(w.form().error && !w.form().running);
    w.set(run::IMAGE, "nginx:alpine");
    w.h.press_key(KeyPress::named(NamedKey::Enter));
    w.wake();
    assert!(!w.open(), "Enter in a text field sends it");
    assert!(w.toasted("Started web2"));
}

/// No containers: Run container is the first step; the terminal stays as another way.
#[test]
fn the_empty_page_offers_run() {
    let mut w = Window::new(Scenario::Empty, Page::Containers);
    assert!(w.app().lists.containers.is_empty());
    assert!(w.h.shows("Run an image to start one."));
    assert!(w.h.hitbox(run::EMPTY_OPEN).is_some(), "Run container");
    assert!(w.h.shows("run -d"), "the terminal command is still there");
    w.click(run::EMPTY_OPEN);
    assert!(w.open(), "the form opens");
}

/// An engine that does not answer has no form.
#[test]
fn no_form_while_the_engine_is_down() {
    let mut w = Window::new(Scenario::Offline, Page::Containers);
    run::open(&mut w.h.view_mut().app, "");
    assert!(!w.open());
}
