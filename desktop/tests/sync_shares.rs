//! Synchronized shares of the macOS VM, in Settings, on the mock backend: add one, sync
//! it, stop syncing it after a question while the other stays; an error shows; the
//! section waits for the VM, and shows only for podbag on a Mac.

use podbag_desktop::dialogs::{self, Dialog};
use podbag_desktop::views::settings;
use podbag_desktop::views::sync_shares::{ADD, NAME, PATH, REFRESH, now_id, remove_id};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Calls, Options as MockOptions, Scenario};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, NamedKey, Synthetic};

type H = Harness<UiView<Desktop>>;

/// Settings; the synchronized shares' section open when it shows.
fn window(calls: &Calls, opts: MockOptions) -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        sync_share: true,
        calls: calls.clone(),
        ..opts
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::new(UiView::new(app), 1200., 1400.);
    h.click_id(shell::nav_id(Page::Settings));
    h.message(Msg::Wake);
    h.repaint();
    let section = settings::section_id("syncShares");
    if h.hitbox(section).is_some() {
        h.click_id(section);
    }
    h
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn set(h: &mut H, id: Id, text: &str) {
    h.click_id(id);
    let m = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL
    };
    h.press_key(KeyPress::char("a").with_mods(m));
    h.press_key(KeyPress::named(NamedKey::Backspace));
    h.input(Synthetic::Text(text.into()));
}

fn names(h: &H) -> Vec<String> {
    let shares = app(h).sync.shares.clone().unwrap_or_default();
    shares.into_iter().map(|s| s.name).collect()
}

#[test]
fn add_sync_and_stop_a_share() {
    let calls = Calls::default();
    let mut h = window(&calls, MockOptions::default());
    assert_eq!(
        names(&h),
        ["project"],
        "the list is read when the section opens"
    );
    assert!(h.shows("podbag-sync-project"));
    assert!(h.shows("/Users/me/projects/app"));

    set(&mut h, NAME, "second");
    set(&mut h, PATH, "/Users/me/Second Project");
    h.click_id(ADD);
    assert_eq!(names(&h), ["project", "second"]);
    assert!(h.shows("podbag-sync-second"));
    assert_eq!(app(&h).sync.name.content(), "", "the form empties");
    assert_eq!(app(&h).sync.path.content(), "");

    h.click_id(now_id("second"));
    assert!(!app(&h).sync.loading);
    assert!(app(&h).sync.error.is_empty());

    h.click_id(remove_id("second"));
    let Some(Dialog::Confirm { text, .. }) = &app(&h).dialog else {
        panic!("Stop syncing asks first");
    };
    assert_eq!(
        text,
        "Stop syncing second? The cache volume and its files are kept."
    );
    h.click_id(dialogs::YES);
    assert_eq!(names(&h), ["project"], "the other share stays");
    assert!(!h.shows("podbag-sync-second"));
    assert_eq!(
        calls.of("vm_sync"),
        ["list", "add second", "now second", "remove second"]
    );

    h.click_id(REFRESH);
    assert_eq!(calls.of("vm_sync").last().map(String::as_str), Some("list"));
}

#[test]
fn errors_show_and_both_fields_are_needed() {
    let calls = Calls::default();
    let mut h = window(&calls, MockOptions::default());
    // Without a folder, nothing is sent and the folder gets the focus.
    set(&mut h, NAME, "project");
    h.press_key(KeyPress::named(NamedKey::Enter));
    assert_eq!(h.focused(), Some(PATH));
    assert_eq!(calls.of("vm_sync"), ["list"]);

    set(&mut h, PATH, "/Users/me/projects/app");
    h.press_key(KeyPress::named(NamedKey::Enter));
    assert_eq!(app(&h).sync.error, "Share already exists");
    assert!(h.shows("Share already exists"));
    assert_eq!(
        app(&h).sync.name.content(),
        "project",
        "the form keeps what was typed"
    );
}

#[test]
fn the_section_waits_for_the_vm() {
    let calls = Calls::default();
    let h = window(
        &calls,
        MockOptions {
            scenario: Scenario::VmStopped,
            ..MockOptions::default()
        },
    );
    assert!(h.shows("Start the Podbag VM to manage synchronized shares."));
    assert!(h.hitbox(ADD).is_none());
    assert!(calls.of("vm_sync").is_empty(), "a stopped VM is not asked");
}

#[test]
fn only_podbag_on_a_mac_has_synchronized_shares() {
    for opts in [
        MockOptions {
            platform: Some("windows"),
            ..MockOptions::default()
        },
        MockOptions {
            platform: Some("linux"),
            ..MockOptions::default()
        },
        MockOptions {
            scenario: Scenario::Docker,
            ..MockOptions::default()
        },
    ] {
        let calls = Calls::default();
        let h = window(&calls, opts);
        assert!(h.hitbox(settings::section_id("syncShares")).is_none());
        assert!(!h.shows("Synchronized shares"));
    }
}
