//! The window on the mock backend, driven through the headless harness: navigation,
//! container actions, dialogs, the pull panel, languages, search and logs.

use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, Event, Kind};
use podbag_desktop_native::app::{KubeKind, Tab};
use podbag_desktop_native::views::{detail, pull, resources, settings};
use podbag_desktop_native::{Desktop, Options, Page, dialogs, shell};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, NamedKey, Synthetic};

type H = Harness<UiView<Desktop>>;

/// The window as it opens (on Migrate, the start page).
fn start_window(opts: MockOptions) -> H {
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

/// The window on Containers.
fn window_with(opts: MockOptions) -> H {
    let mut h = start_window(opts);
    h.view_mut().app.go(Page::Containers);
    h.repaint();
    h
}

fn window() -> H {
    window_with(MockOptions::default())
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn row(kind: Kind, i: usize) -> Id {
    resources::table_id(kind).with(("row", i))
}

/// Run what the backend reported (the window gets a wake-up for it).
fn wake(h: &mut H) {
    h.message(podbag_desktop_native::Msg::Wake);
}

fn container_state(h: &H, name: &str) -> String {
    let c = app(h).lists.containers.iter().find(|c| c.name() == name);
    c.map(|c| c.state.clone()).unwrap_or_default()
}

fn type_text(h: &mut H, text: &str) {
    h.input(Synthetic::Text(text.into()));
}

fn key(h: &mut H, k: NamedKey) {
    h.press_key(KeyPress::named(k));
}

#[test]
fn every_view_is_reached_from_the_sidebar_and_history_goes_back() {
    let h = start_window(MockOptions::default());
    assert_eq!(app(&h).page, Page::Migrate, "Migrate is the start page");
    let mut h = window();
    assert!(app(&h).loaded, "the mock's lists load at start");
    assert_eq!(app(&h).page, Page::Containers);
    // Training lists the synced modules; the whole sidebar fits a taller window.
    h.resize(1200., 1100.);
    for page in [
        Page::Images,
        Page::Volumes,
        Page::Networks,
        Page::Training,
        Page::Migrate,
        Page::Kubernetes,
        Page::Settings,
        Page::Containers,
    ] {
        h.click_id(shell::nav_id(page));
        assert_eq!(app(&h).page, page, "{page:?}");
    }
    // Kubernetes and its resources.
    h.click_id(shell::nav_id(Page::Kubernetes));
    assert!(app(&h).kube_expanded);
    h.click_id(shell::kube_nav_id(KubeKind::Services));
    assert_eq!(app(&h).kube.kind, KubeKind::Services);
    assert!(!app(&h).kube.items.is_empty(), "the mock has services");
    // The top bar's settings, then back and forward.
    h.click_id(shell::TOP_SETTINGS);
    assert_eq!(app(&h).page, Page::Settings);
    h.click_id(shell::NAV_BACK);
    assert_eq!(app(&h).page, Page::Kubernetes);
    assert_eq!(app(&h).kube.kind, KubeKind::Services);
    h.click_id(shell::NAV_BACK);
    assert_eq!(app(&h).kube.kind, KubeKind::Pods);
    h.click_id(shell::NAV_FORWARD);
    assert_eq!(app(&h).kube.kind, KubeKind::Services);
    // The keyboard: Cmd/Ctrl+[ or Alt+Left goes back too.
    let back = if cfg!(target_os = "macos") {
        KeyPress::char("[").with_mods(ModifiersState::SUPER)
    } else {
        KeyPress::named(NamedKey::ArrowLeft).with_mods(ModifiersState::ALT)
    };
    h.press_key(back);
    assert_eq!(app(&h).kube.kind, KubeKind::Pods);
}

#[test]
fn a_container_stops_and_starts_again() {
    let mut h = window();
    assert_eq!(app(&h).lists.containers[0].name(), "web");
    assert_eq!(container_state(&h, "web"), "running");
    h.click_id(row(Kind::Containers, 0).with("stop"));
    assert_eq!(container_state(&h, "web"), "exited");
    assert!(!app(&h).busy.contains(&app(&h).lists.containers[0].id));
    h.click_id(row(Kind::Containers, 0).with("start"));
    assert_eq!(container_state(&h, "web"), "running");
}

#[test]
fn remove_asks_first_and_force_removes_a_running_container() {
    let mut h = window();
    let before = app(&h).lists.containers.len();
    // A stopped one: Cancel keeps it.
    let worker = app(&h)
        .lists
        .containers
        .iter()
        .position(|c| c.name() == "worker")
        .unwrap();
    h.click_id(row(Kind::Containers, worker).with("remove"));
    assert!(app(&h).dialog.is_some());
    assert_eq!(h.focused(), Some(dialogs::CANCEL), "Cancel has the focus");
    // Enter on the focused Cancel closes it, as in the web page.
    key(&mut h, NamedKey::Enter);
    assert!(app(&h).dialog.is_none());
    assert_eq!(app(&h).lists.containers.len(), before);
    // Again, and Remove.
    h.click_id(row(Kind::Containers, worker).with("remove"));
    h.click_id(dialogs::YES);
    assert!(app(&h).dialog.is_none());
    assert_eq!(app(&h).lists.containers.len(), before - 1);
    assert!(
        app(&h)
            .lists
            .containers
            .iter()
            .all(|c| c.name() != "worker")
    );
    // A running one is removed with force.
    h.click_id(row(Kind::Containers, 0).with("remove"));
    h.click_id(dialogs::YES);
    assert!(app(&h).lists.containers.iter().all(|c| c.name() != "web"));
}

#[test]
fn rename_keeps_tab_in_the_dialog_and_escape_closes_it() {
    let mut h = window();
    h.click_id(row(Kind::Containers, 0));
    assert_eq!(app(&h).detail.as_ref().map(|d| d.tab), Some(Tab::Info));
    h.click_id(detail::ACTIONS.with("rename"));
    assert!(app(&h).dialog.is_some());
    assert_eq!(
        h.focused(),
        Some(dialogs::INPUT),
        "the input has the focus once"
    );
    // Tab goes round the dialog's widgets only.
    let mut seen = Vec::new();
    for _ in 0..6 {
        h.tab();
        seen.push(h.focused().unwrap());
    }
    for id in &seen {
        assert!(
            [dialogs::INPUT, dialogs::CANCEL, dialogs::YES].contains(id),
            "{id:?} is outside the dialog"
        );
    }
    assert!(seen.contains(&dialogs::INPUT) && seen.contains(&dialogs::YES));
    h.shift_tab();
    assert!(app(&h).dialog.is_some());
    // Escape closes it; the page stays.
    key(&mut h, NamedKey::Escape);
    assert!(app(&h).dialog.is_none());
    assert!(app(&h).detail.is_some(), "Escape closed only the dialog");
    // Again: type a name and press Enter.
    h.click_id(detail::ACTIONS.with("rename"));
    type_text(&mut h, "frontend");
    key(&mut h, NamedKey::Enter);
    assert!(app(&h).dialog.is_none());
    assert_eq!(app(&h).lists.containers[0].name(), "frontend");
    // A name in use: the dialog stays with the error.
    h.click_id(detail::ACTIONS.with("rename"));
    type_text(&mut h, "db");
    key(&mut h, NamedKey::Enter);
    match &app(&h).dialog {
        Some(dialogs::Dialog::Ask { error, .. }) => assert!(error.contains("in use"), "{error}"),
        _ => panic!("the dialog closed"),
    }
    key(&mut h, NamedKey::Escape);
    // Escape on the page closes the details.
    key(&mut h, NamedKey::Escape);
    assert!(app(&h).detail.is_none());
}

#[test]
fn a_pull_runs_to_done() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Images));
    let before = app(&h).lists.images.len();
    h.click_id(Id::of("pull"));
    assert!(app(&h).pull.is_some());
    assert_eq!(h.focused(), Some(pull::INPUT));
    // A search, then a result, its tag, and the pull.
    type_text(&mut h, "nginx");
    let p = app(&h).pull.as_ref().unwrap();
    assert!(!p.results.is_empty(), "Docker Hub results");
    key(&mut h, NamedKey::ArrowDown);
    key(&mut h, NamedKey::Enter);
    assert_eq!(app(&h).pull.as_ref().unwrap().step, pull::Step::Tag);
    assert_eq!(h.focused(), Some(pull::TAG));
    // Escape steps back to the search.
    key(&mut h, NamedKey::Escape);
    assert_eq!(app(&h).pull.as_ref().unwrap().step, pull::Step::Search);
    // A full reference pulls at once.
    let input = &mut h.view_mut().app.pull.as_mut().unwrap().input;
    input.set_content("");
    type_text(&mut h, "busybox:1.37");
    key(&mut h, NamedKey::Enter);
    wake(&mut h);
    let p = app(&h).pull.as_ref().unwrap();
    assert_eq!(p.step, pull::Step::Done);
    assert_eq!(p.result, Some(Ok("busybox:1.37".to_string())));
    assert!(p.lines.len() > 3, "progress lines: {:?}", p.lines);
    assert_eq!(h.focused(), Some(pull::CLOSE));
    assert_eq!(app(&h).lists.images.len(), before + 1);
    key(&mut h, NamedKey::Enter);
    assert!(app(&h).pull.is_none(), "Enter on Close closes the panel");
}

#[test]
fn the_language_switches_to_korean_and_back() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Settings));
    h.click_id(settings::LANGUAGE);
    // The menu: system, then en, ko, …
    h.click_id(settings::LANGUAGE.with(("item", 2usize)));
    assert_eq!(app(&h).tr.lang, "ko");
    assert_eq!(app(&h).t("nav.containers"), "컨테이너");
    assert_eq!(app(&h).api.settings().language, "ko", "saved");
    h.click_id(settings::LANGUAGE);
    h.click_id(settings::LANGUAGE.with(("item", 1usize)));
    assert_eq!(app(&h).tr.lang, "en");
    assert_eq!(app(&h).t("nav.containers"), "Containers");
}

#[test]
fn search_filters_the_list_and_ctrl_f_focuses_it() {
    let mut h = window();
    let search = resources::search_id(Kind::Containers);
    h.press_key(KeyPress::shortcut('f'));
    assert_eq!(h.focused(), Some(search));
    type_text(&mut h, "postgres");
    assert!(h.hitbox(row(Kind::Containers, 0)).is_some());
    assert!(h.hitbox(row(Kind::Containers, 1)).is_none(), "one row left");
    h.click_id(row(Kind::Containers, 0));
    let id = app(&h).detail.as_ref().map(|d| d.id.clone()).unwrap();
    assert!(id.starts_with("b2c3"), "the db container: {id}");
    key(&mut h, NamedKey::Escape);
    h.click_id(search);
    type_text(&mut h, "zzz");
    assert!(
        h.hitbox(row(Kind::Containers, 0)).is_none(),
        "nothing matches"
    );
}

#[test]
fn logs_follow_and_wrap() {
    let mut h = window();
    h.click_id(row(Kind::Containers, 0).with("logs"));
    assert_eq!(app(&h).detail.as_ref().map(|d| d.tab), Some(Tab::Logs));
    wake(&mut h);
    assert!(!app(&h).logs.lines.is_empty(), "the sample lines came");
    assert!(app(&h).logs.follow && app(&h).logs.wrap);
    h.click_id(detail::FOLLOW);
    assert!(!app(&h).logs.follow);
    h.click_id(detail::WRAP);
    assert!(!app(&h).logs.wrap);
    // Lines from another stream are not shown.
    let n = app(&h).logs.lines.len();
    let session = app(&h).session;
    app(&h).api.events().send(Event::Logs {
        session,
        generation: 999,
        lines: vec!["other".into()],
    });
    wake(&mut h);
    assert_eq!(app(&h).logs.lines.len(), n);
    // The Info tab.
    h.click_id(detail::TABS.with(0usize));
    assert_eq!(app(&h).detail.as_ref().map(|d| d.tab), Some(Tab::Info));
}

#[test]
fn events_of_an_old_engine_session_are_dropped() {
    let mut h = window();
    let old = app(&h).session;
    h.view_mut().app.switch_engine("docker").unwrap();
    wake(&mut h);
    assert_ne!(app(&h).session, old);
    let live = app(&h).live;
    app(&h).api.events().send(Event::Live {
        session: old,
        on: !live,
    });
    wake(&mut h);
    assert_eq!(app(&h).live, live, "an old session's event changed nothing");
}

#[test]
fn offline_and_stopped_vm_pages_offer_their_way_out() {
    let mut h = window_with(MockOptions {
        scenario: Scenario::VmStopped,
        ..MockOptions::default()
    });
    assert!(!app(&h).reachable());
    assert!(h.hitbox(Id::of("offline-start")).is_some());
    h.click_id(Id::of("offline-start"));
    assert!(app(&h).reachable(), "Start VM started it");
    assert!(app(&h).loaded);

    let h = window_with(MockOptions {
        scenario: Scenario::Offline,
        ..MockOptions::default()
    });
    assert!(!app(&h).reachable());
    assert!(h.hitbox(Id::of("offline-retry")).is_some());
    assert!(
        h.hitbox(Id::of("offline-code")).is_some(),
        "a command to copy"
    );
}

#[test]
fn tab_goes_through_the_sidebar_then_the_toolbar_then_the_rows() {
    let mut h = window();
    let mut order = Vec::new();
    for _ in 0..40 {
        h.tab();
        order.push(h.focused().unwrap());
    }
    let at = |id: Id| order.iter().position(|x| *x == id).unwrap();
    let containers = at(shell::nav_id(Page::Containers));
    let settings = at(shell::nav_id(Page::Settings));
    let search = at(resources::search_id(Kind::Containers));
    let first_row = at(row(Kind::Containers, 0));
    let stop = at(row(Kind::Containers, 0).with("stop"));
    assert!(at(shell::SIDEBAR_TOGGLE) < containers);
    assert!(containers < settings && settings < search);
    assert!(search < first_row && first_row < stop);
    // Enter on a row's name opens it.
    while h.focused() != Some(row(Kind::Containers, 0)) {
        h.tab();
    }
    key(&mut h, NamedKey::Enter);
    assert!(app(&h).detail.is_some());
}

#[test]
fn the_sidebar_collapses_to_a_rail_and_back() {
    let mut h = window();
    assert!(!app(&h).collapsed());
    h.click_id(shell::SIDEBAR_TOGGLE);
    assert!(app(&h).collapsed());
    assert!(app(&h).api.settings().sidebar_collapsed, "saved");
    h.click_id(shell::SIDEBAR_TOGGLE);
    assert!(!app(&h).collapsed());
    // Dragging the divider resizes it.
    let r = h.hitbox(shell::SPLITTER).unwrap();
    let from = podbag_ui::Point::new(r.x + 4., 300.);
    h.drag(from, podbag_ui::Point::new(from.x + 60., 300.));
    assert_eq!(app(&h).settings.sidebar_width, 276);
}
