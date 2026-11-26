//! Training, Migrate, updates, registry login, the import from Docker and the notices,
//! on the mock backend through the headless harness.

use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Options as MockOptions, Scenario, sample_module};
use podbag_desktop_native::views::{
    import, login, migrate, notices, practice, pull, settings, training, updates,
};
use podbag_desktop_native::{Desktop, Msg, Options, Page, dialogs, shell};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{KeyPress, ModifiersState, NamedKey, Synthetic};

type H = Harness<UiView<Desktop>>;

fn window_with(opts: MockOptions) -> H {
    podbag_desktop_native::icons::install();
    let opts = MockOptions {
        instant: true,
        all_features: true,
        ..opts
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    Harness::new(UiView::new(app), 1200., 800.)
}

fn window() -> H {
    window_with(MockOptions::default())
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn wake(h: &mut H) {
    h.message(Msg::Wake);
}

fn type_text(h: &mut H, text: &str) {
    h.input(Synthetic::Text(text.into()));
}

fn select_all(h: &mut H) {
    let m = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL
    };
    h.press_key(KeyPress::char("a").with_mods(m));
}

/// A file in the temp folder with `text`, for "Install module".
fn module_file(name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("podbag-views-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lesson.podbag");
    std::fs::write(&path, text).unwrap();
    path
}

/// "Install module", the file's path, then Install in the confirmation.
fn install(h: &mut H, path: &std::path::Path) {
    h.click_id(training::INSTALL);
    assert!(app(h).dialog.is_some(), "the file is asked for");
    type_text(h, &path.display().to_string());
    h.press_key(KeyPress::named(NamedKey::Enter));
    assert!(
        matches!(app(h).dialog, Some(dialogs::Dialog::Confirm { .. })),
        "Install asks first"
    );
    h.click_id(dialogs::YES);
    wake(h);
}

fn lesson_pack() -> String {
    serde_json::json!({
        "format": "podbag-volume-api-lab-v1",
        "starter": "{\"message\":\"hello\"}",
        "translations": { "en": {
            "title": "Lab 1 · My HTTP lab", "goal": "Serve a file.", "create": "Name a volume.",
            "write": "Write JSON.", "serve": "Start the service.", "request": "Send a request.",
        } },
    })
    .to_string()
}

#[test]
fn migrate_is_the_start_page_and_its_sidebar_counts_what_it_found() {
    let h = window();
    assert_eq!(app(&h).page, Page::Migrate);
    let m = &app(&h).migrate;
    assert_eq!(m.pending, 0);
    assert!(m.counts["containers"] > 0 && m.counts["volumes"] > 0);
    assert!(
        h.hitbox(migrate::nav_id("volumes")).is_some(),
        "kinds with rows show"
    );
    assert!(h.hitbox(migrate::nav_id("kube")).is_some());
}

#[test]
fn a_migration_runs_from_the_selection_to_done() {
    let mut h = window();
    h.click_id(migrate::nav_id("volumes"));
    assert_eq!(app(&h).migrate.kind, "volumes");
    let row = migrate::row_id("volumes", "docker", "pgdata");
    assert!(h.hitbox(row).is_some(), "only the volumes show");
    h.click_id(row);
    let Some(dialogs::Dialog::Migration(m)) = &app(&h).dialog else {
        panic!("the migration dialog opens");
    };
    assert_eq!(m.phase, migrate::Phase::Confirm);
    assert_eq!(m.item.source, "docker");
    h.click_id(migrate::START);
    wake(&mut h);
    let Some(dialogs::Dialog::Migration(m)) = &app(&h).dialog else {
        panic!("the dialog stays open");
    };
    assert_eq!(m.phase, migrate::Phase::Done);
    assert_eq!(m.destination, "Podbag");
    h.click_id(dialogs::YES);
    assert!(app(&h).dialog.is_none());
    // The kubernetes group lists the cluster's resources.
    h.click_id(migrate::nav_id("kube"));
    assert!(app(&h).migrate.cluster);
    assert!(app(&h).migrate.counts["pods"] > 0);
}

#[test]
fn a_tutorial_runs_step_by_step_to_done_and_its_lab_resets() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Training));
    let path = module_file("lesson", &lesson_pack());
    install(&mut h, &path);
    assert_eq!(
        app(&h).training.view.as_deref(),
        Some(training::JSON_LESSON)
    );
    assert!(
        h.hitbox(training::module_nav_id(training::JSON_LESSON))
            .is_some()
    );
    h.click_id(training::LAB_START);
    wake(&mut h);
    // Create a volume.
    h.click_id(training::LAB_NAME);
    type_text(&mut h, "my-api");
    h.click_id(training::LAB_CREATE);
    wake(&mut h);
    let lab = app(&h).training.lab.clone().unwrap();
    assert_eq!(lab.volume.as_deref(), Some("my-api"));
    // The file starts as the lesson's starter.
    h.repaint();
    assert_eq!(
        app(&h).training.lab_source.content(),
        "{\"message\":\"hello\"}"
    );
    h.click_id(training::LAB_SAVE);
    wake(&mut h);
    h.click_id(training::LAB_SERVE);
    wake(&mut h);
    assert!(app(&h).training.lab.as_ref().unwrap().url.is_some());
    h.click_id(training::LAB_SEND);
    wake(&mut h);
    let lab = app(&h).training.lab.clone().unwrap();
    assert_eq!(lab.response, Some((200, "{\"message\":\"hello\"}".into())));
    assert!(app(&h).training.lesson.as_ref().unwrap().complete);
    assert_eq!(app(&h).api.saved_lesson().unwrap()["complete"], true);
    // End lab asks, then the lab starts over.
    h.click_id(training::LAB_END);
    h.click_id(dialogs::YES);
    wake(&mut h);
    assert!(!app(&h).training.lab.as_ref().unwrap().active);
    assert!(h.hitbox(training::LAB_START).is_some());
    // Removing it leaves the modules.
    h.click_id(training::remove_id(training::JSON_LESSON));
    h.click_id(dialogs::YES);
    assert!(app(&h).training.lesson.is_none());
    assert_eq!(app(&h).training.view, None);
}

#[test]
fn a_practice_session_is_followed_from_its_start_to_its_end() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Training));
    let path = module_file("containers", &sample_module("container-basics").unwrap());
    install(&mut h, &path);
    assert_eq!(app(&h).training.view.as_deref(), Some("container-basics"));
    let stage = |h: &H| app(h).training.stages["container-basics"].stage.clone();
    assert_eq!(stage(&h), "start");
    // The terminal starts the session (`podbag lab start container-basics`) and runs
    // the container.
    let api = app(&h).api.clone();
    api.practice("start", None, Some("container-basics"), false, false)
        .unwrap();
    training::poll(&mut h.view_mut().app);
    wake(&mut h);
    assert_eq!(stage(&h), "run");
    api.practice("advance", None, None, true, false).unwrap();
    // Practice containers shows it; its Stop is the window's step.
    h.click_id(shell::nav_id(Page::LabContainers));
    assert_eq!(app(&h).page, Page::LabContainers);
    let stop = practice::row_id(0).with("action");
    assert!(h.hitbox(stop).is_some(), "Stop is offered at the stop step");
    h.click_id(stop);
    wake(&mut h);
    let state = app(&h).practice.state.clone().unwrap();
    assert_eq!(state["stage"], "check");
    assert_eq!(state["containers"][0]["State"], "exited");
    // The terminal checks and ends the session: the module is complete.
    api.practice("advance", None, None, true, false).unwrap();
    api.practice("advance", None, None, true, false).unwrap();
    practice::poll(&mut h.view_mut().app);
    wake(&mut h);
    assert!(app(&h).practice.state.as_ref().unwrap().is_null());
    h.repaint();
    assert!(
        h.hitbox(podbag_ui::Id::of(("practice-start", "container-basics")))
            .is_some(),
        "without a session, the command that starts one"
    );
    h.click_id(training::module_nav_id("container-basics"));
    wake(&mut h);
    assert_eq!(stage(&h), "complete");
    assert!(app(&h).training.records[0].completed);
}

#[test]
fn training_syncs_at_start_and_on_refresh() {
    let mut h = window();
    let ids: Vec<String> = app(&h)
        .training
        .records
        .iter()
        .map(|r| r.id.clone())
        .collect();
    assert!(ids.contains(&"web-service-recovery".to_string()), "{ids:?}");
    let synced = app(&h).training.synced_at.unwrap();
    // The page syncs at most once a minute; Refresh always does.
    h.click_id(shell::nav_id(Page::Training));
    wake(&mut h);
    assert_eq!(app(&h).training.synced_at, Some(synced));
    h.click_id(training::REFRESH);
    wake(&mut h);
    assert!(app(&h).training.synced_at.unwrap() > synced);
    assert!(!app(&h).training.syncing);
    assert!(app(&h).training.status.is_empty());
}

#[test]
fn offline_a_refresh_says_why_and_the_modules_stay() {
    let mut h = window_with(MockOptions {
        scenario: Scenario::Offline,
        ..MockOptions::default()
    });
    assert!(app(&h).training.records.is_empty());
    h.click_id(shell::nav_id(Page::Training));
    let path = module_file("offline", &sample_module("container-basics").unwrap());
    install(&mut h, &path);
    h.click_id(training::REFRESH);
    wake(&mut h);
    let status = app(&h).training.status.clone();
    assert!(
        status.starts_with("Could not refresh the Training list: cannot reach"),
        "{status}"
    );
    let ids: Vec<&str> = app(&h)
        .training
        .records
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(ids, ["container-basics"]);
}

#[test]
fn course_steps_are_named_by_the_module_file() {
    let mut h = window();
    h.view_mut().app.command("lang ko");
    let d = app(&h);
    let web = d
        .training
        .records
        .iter()
        .find(|r| r.id == "web-basics")
        .unwrap();
    let p = training::practice_for(web).unwrap();
    assert_eq!(p.label(d, "runWeb"), "웹 서버 실행하기");
    assert_eq!(p.label(d, "start"), d.t("training.course.start"));
    assert_eq!(p.label(d, "missing"), "missing");
    // Without the language, English.
    let mut p = p.clone();
    p.steps[1].names.remove("ko");
    assert_eq!(p.label(d, "runWeb"), "Run the web server");
}

#[test]
fn opening_the_failed_container_is_the_inspect_step() {
    let mut h = window();
    let api = app(&h).api.clone();
    api.practice("start", None, Some("web-service-recovery"), false, false)
        .unwrap();
    api.practice("advance", None, None, false, true).unwrap();
    h.click_id(shell::nav_id(Page::LabContainers));
    wake(&mut h);
    let state = app(&h).practice.state.clone().unwrap();
    assert_eq!(state["stage"], "inspectFailed");
    h.click_id(practice::row_id(0));
    wake(&mut h);
    let state = app(&h).practice.state.clone().unwrap();
    assert_eq!(state["stage"], "readRecoveryLogs");
    assert!(
        app(&h).practice.container.is_some(),
        "the container is shown"
    );
}

#[test]
fn an_update_check_shows_the_new_version_and_update_needs_an_administrator() {
    let mut h = window();
    // Tall enough to show the whole section.
    h.resize(1200., 1400.);
    h.click_id(shell::TOP_SETTINGS);
    h.click_id(settings::section_id("updates"));
    assert!(!app(&h).updates.as_ref().unwrap().newer);
    assert!(h.hitbox(podbag_ui::Id::of("status-update")).is_none());
    h.click_id(updates::CHECK);
    wake(&mut h);
    let u = app(&h).updates.clone().unwrap();
    assert!(u.newer);
    assert_eq!(u.latest.as_deref(), Some("0.2.0"));
    h.repaint();
    assert!(h.hitbox(podbag_ui::Id::of("status-update")).is_some());
    // Update: the terminal's podbag needs the password; the app can update now.
    h.click_id(updates::INSTALL);
    wake(&mut h);
    assert!(app(&h).update.upgrade.as_ref().unwrap().needs_admin);
    assert!(h.hitbox(updates::OPEN_TERMINAL).is_some());
    h.click_id(updates::APP_NOW);
    wake(&mut h);
    std::thread::sleep(std::time::Duration::from_millis(600));
    h.timer();
    h.repaint();
    assert!(h.quit_requested(), "the app ends for the new one");
}

#[test]
fn a_registry_login_shows_its_error_then_logs_in() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Images));
    h.view_mut().app.open_pull();
    h.repaint();
    h.click_id(pull::LOCATION);
    h.click_id(login::menu_login_id("quay"));
    assert_eq!(app(&h).pull.as_ref().unwrap().step, pull::Step::Login);
    assert_eq!(h.focused(), Some(login::USER));
    type_text(&mut h, "denied");
    h.click_id(login::PASSWORD);
    type_text(&mut h, "x");
    assert_eq!(
        app(&h)
            .pull
            .as_ref()
            .unwrap()
            .login
            .password
            .input
            .content(),
        "•",
        "the password shows dots"
    );
    h.click_id(login::SUBMIT);
    wake(&mut h);
    let p = app(&h).pull.as_ref().unwrap();
    assert_eq!(p.step, pull::Step::Login);
    assert!(p.login.status.contains("unauthorized"));
    assert!(p.login.password.is_empty(), "the password is not kept");
    h.click_id(login::USER);
    select_all(&mut h);
    type_text(&mut h, "someone");
    h.click_id(login::PASSWORD);
    type_text(&mut h, "y");
    h.press_key(KeyPress::named(NamedKey::Enter));
    wake(&mut h);
    let p = app(&h).pull.as_ref().unwrap();
    assert_eq!(p.step, pull::Step::Search);
    assert!(!app(&h).toasts.is_empty(), "Logged in");
}

#[test]
fn docker_images_are_imported() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Images));
    h.click_id(import::OPEN);
    let Some(dialogs::Dialog::Import(m)) = &app(&h).dialog else {
        panic!("the import dialog opens");
    };
    assert_eq!(m.images.len(), 6);
    assert!(m.have[0], "nginx:alpine is here already");
    h.click_id(import::row_id(1));
    h.click_id(import::row_id(2));
    let Some(dialogs::Dialog::Import(m)) = &app(&h).dialog else {
        panic!("still open");
    };
    assert_eq!(m.checked, [false, true, true, false, false, false]);
    h.click_id(import::COPY);
    wake(&mut h);
    assert!(app(&h).dialog.is_none());
    assert!(!app(&h).toasts.is_empty(), "Copied images: 2");
}

#[test]
fn the_star_prompt_then_the_terminal_notice_until_not_now() {
    let mut h = window_with(MockOptions {
        first_launch: true,
        ..MockOptions::default()
    });
    assert!(matches!(app(&h).dialog, Some(dialogs::Dialog::Star)));
    assert_eq!(h.focused(), Some(dialogs::CANCEL), "Skip has the focus");
    h.click_id(dialogs::CANCEL);
    wake(&mut h);
    assert!(app(&h).dialog.is_none());
    // The mock's terminal runs a separate podbag: no notice.
    assert!(app(&h).notices.cli_notice.is_none());
    assert!(app(&h).notices.cli.is_some());
    // When terminals find no podbag, the notice offers the app's own.
    h.view_mut().app.command("notice cli");
    h.repaint();
    assert!(h.hitbox(notices::CLI_INSTALL).is_some());
    h.click_id(notices::CLI_LATER);
    assert!(app(&h).notices.cli_notice.is_none());
    assert!(app(&h).settings.cli_notice_dismissed);
    assert!(app(&h).api.settings().cli_notice_dismissed, "saved");
}
