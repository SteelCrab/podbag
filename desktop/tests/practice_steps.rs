//! Module `ui` steps: Training says where to do a step in the window, and the practice
//! pages do it by the name the module gives, on the mock backend through the headless
//! harness.

use podbag_desktop::views::{practice, training};
use podbag_desktop::{Desktop, Msg, Options, Page, dialogs, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::Options as MockOptions;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use serde_json::{Value, json};

type H = Harness<UiView<Desktop>>;

fn window() -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        all_features: true,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    Harness::new(UiView::new(app), 1200., 900.)
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn wake(h: &mut H) {
    h.message(Msg::Wake);
}

fn state(h: &H) -> Value {
    app(h).practice.state.clone().unwrap()
}

/// The text of the open confirmation.
fn question(h: &H) -> String {
    match &app(h).dialog {
        Some(dialogs::Dialog::Confirm { text, .. }) => text.clone(),
        other => panic!("no confirmation: {}", other.is_some()),
    }
}

/// The terminal starts a session of `id` (`podbag lab start <id>`) and does `steps` of
/// its terminal steps.
fn start(h: &H, id: &str, steps: usize) {
    let api = app(h).api.clone();
    api.practice("start", None, Some(id), false, false).unwrap();
    for _ in 0..steps {
        api.practice("advance", None, None, false, true).unwrap();
    }
}

/// web-service-recovery, at its logs step: the failed container was opened.
fn at_the_logs_step(h: &mut H) {
    start(h, "web-service-recovery", 1);
    h.click_id(shell::nav_id(Page::LabContainers));
    wake(h);
    h.click_id(practice::row_id(0));
    wake(h);
    assert_eq!(state(h)["stage"], "readRecoveryLogs");
    h.click_id(practice::DETAIL_BACK);
}

#[test]
fn training_says_where_each_ui_step_is_done() {
    let mut h = window();
    start(&h, "web-service-recovery", 1);
    let api = app(&h).api.clone();
    let s = api.practice("status", None, None, false, true).unwrap();
    let session = s["session"].as_str().unwrap();
    let id = s["containers"][0]["Id"].as_str().unwrap();
    api.practice("inspect-failed", Some(session), Some(id), false, true)
        .unwrap();
    h.click_id(shell::nav_id(Page::Training));
    h.click_id(training::module_nav_id("web-service-recovery"));
    wake(&mut h);
    h.repaint();
    // The current step: the window's way, then the command.
    assert!(h.shows("In Podbag Desktop: Practice containers → lab-web → Logs"));
    assert!(h.shows("Or run this command in the training shell:"));
    assert!(h.shows("podbag logs lab-web"));
    // A later step says it too, from the module.
    assert!(h.shows("In Podbag Desktop: Practice containers → lab-web → Remove"));
}

#[test]
fn a_row_step_is_done_by_name_and_its_log_stays_under_its_container() {
    let mut h = window();
    at_the_logs_step(&mut h);
    // The named row has the step's Logs.
    let logs = practice::row_id(0).with("action");
    assert!(h.hitbox(logs).is_some(), "Logs is on the lab-web row");
    h.click_id(logs);
    wake(&mut h);
    let log = app(&h).practice.log.clone().expect("the log text");
    assert!(log.text.contains("SITE_TITLE is required"), "{}", log.text);
    assert_eq!(log.stage, "removeFailed");
    assert_eq!(
        app(&h).practice.container.as_ref(),
        Some(&log.id),
        "the container is shown"
    );
    h.repaint();
    assert!(h.shows("SITE_TITLE is required"));
    // The next step removes it: the header asks first, about the stopped container.
    h.click_id(practice::DETAIL_ACTION);
    let want = app(&h).tv("confirm.removeContainer", &[("name", "lab-web")]);
    assert_eq!(question(&h), want);
    h.click_id(dialogs::YES);
    wake(&mut h);
    let s = state(&h);
    assert_eq!(s["stage"], "runFixed");
    assert_eq!(s["containers"], json!([]));
    assert!(
        app(&h).practice.log.is_none(),
        "the log goes with its container"
    );
    h.repaint();
    assert!(
        h.hitbox(practice::DETAIL_BACK).is_none(),
        "the list is shown"
    );
}

#[test]
fn the_log_goes_when_the_step_moves_on() {
    let mut h = window();
    at_the_logs_step(&mut h);
    h.click_id(practice::row_id(0).with("action"));
    wake(&mut h);
    assert!(app(&h).practice.log.is_some());
    // The terminal removes the container and runs the fixed one.
    let api = app(&h).api.clone();
    api.practice("advance", None, None, false, true).unwrap();
    api.practice("advance", None, None, false, true).unwrap();
    practice::poll(&mut h.view_mut().app);
    wake(&mut h);
    assert_eq!(state(&h)["stage"], "ports");
    assert!(app(&h).practice.log.is_none());
}

#[test]
fn a_removal_goes_only_to_the_session_the_learner_confirmed() {
    let mut h = window();
    at_the_logs_step(&mut h);
    h.click_id(practice::row_id(0).with("action"));
    wake(&mut h);
    h.click_id(practice::DETAIL_ACTION);
    assert!(app(&h).dialog.is_some(), "Remove asks first");
    // A new session starts while the question is open.
    start(&h, "web-service-recovery", 1);
    practice::poll(&mut h.view_mut().app);
    wake(&mut h);
    let fresh = state(&h);
    assert_eq!(fresh["stage"], "inspectFailed");
    h.click_id(dialogs::YES);
    wake(&mut h);
    let s = state(&h);
    assert_eq!(s["session"], fresh["session"]);
    assert_eq!(
        s["stage"], "inspectFailed",
        "nothing went to the new session"
    );
    assert_eq!(s["containers"][0]["Names"][0], "/lab-web");
}

#[test]
fn a_toolbar_step_creates_the_volume_without_a_question() {
    let mut h = window();
    start(&h, "volume-persistence", 0);
    h.click_id(shell::nav_id(Page::LabVolumes));
    wake(&mut h);
    assert_eq!(state(&h)["stage"], "create");
    assert!(h.shows("Create volume training-data"));
    h.click_id(practice::STEP);
    assert!(app(&h).dialog.is_none());
    wake(&mut h);
    let s = state(&h);
    assert_eq!(s["stage"], "write");
    assert_eq!(s["volumes"][0]["Name"], "training-data");
    h.repaint();
    assert!(
        h.hitbox(practice::STEP).is_none(),
        "a step without a ui has no button"
    );
}

/// A course whose steps act on a network and on images, which no row of the practice
/// pages shows.
fn network_module() -> String {
    let step = |key: &str, command: &str, ui: Value, check: Value| json!({ "key": key, "names": { "en": key }, "command": command, "ui": ui, "check": check });
    json!({
        "format": "podbag-course-v2",
        "id": "network-steps",
        "translations": { "en": {
            "title": "Podbag Training · Network steps",
            "guide": "Start the session with podbag lab start network-steps.",
            "url": "https://podbag.dev/",
        } },
        "steps": [
            step("inspectNet", "podbag network inspect lab-net",
                json!({ "action": "inspect", "name": "lab-net" }),
                json!({ "kind": "output-contains", "text": "10.89.0.0/24" })),
            step("removeNet", "podbag network rm lab-net",
                json!({ "action": "remove-network", "name": "lab-net" }),
                json!({ "kind": "network-absent", "name": "lab-net" })),
            step("pruneImages", "podbag image prune -a",
                json!({ "action": "prune", "name": "images" }),
                json!({ "kind": "image-absent", "name": "busybox" })),
            { "key": "finish", "names": { "en": "Finish" }, "command": "exit",
              "check": { "kind": "finish" } },
        ],
    })
    .to_string()
}

#[test]
fn toolbar_steps_show_a_network_and_ask_before_they_remove() {
    let mut h = window();
    let api = app(&h).api.clone();
    api.training_modules("install", Some(&network_module()), None)
        .unwrap();
    start(&h, "network-steps", 0);
    h.click_id(shell::nav_id(Page::LabContainers));
    wake(&mut h);
    assert_eq!(state(&h)["stage"], "inspectNet");
    // An inspect of a name no row has: a toolbar button, and the details stay on the page.
    assert!(h.shows("Details: lab-net"));
    h.click_id(practice::STEP);
    assert!(app(&h).dialog.is_none());
    wake(&mut h);
    assert_eq!(state(&h)["stage"], "removeNet");
    let shown = app(&h).practice.inspected.clone().expect("the network");
    assert_eq!(shown.detail["Name"], "lab-net");
    h.repaint();
    assert!(h.shows("10.89.0.0/24"));
    // Remove network asks first; the details go when the step moves on.
    h.click_id(practice::STEP);
    let want = app(&h).tv("confirm.removeNetwork", &[("name", "lab-net")]);
    assert_eq!(question(&h), want);
    h.click_id(dialogs::YES);
    wake(&mut h);
    assert_eq!(state(&h)["stage"], "pruneImages");
    assert!(app(&h).practice.inspected.is_none());
    h.repaint();
    assert!(!h.shows("10.89.0.0/24"));
    // Clean up asks the prune question of its kind.
    assert!(h.shows("Clean up images"));
    h.click_id(practice::STEP);
    assert_eq!(question(&h), app(&h).t("confirm.pruneImages"));
    h.click_id(dialogs::YES);
    wake(&mut h);
    assert_eq!(state(&h)["stage"], "finish");
}
