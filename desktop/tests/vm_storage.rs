use podbag_desktop::views::{settings, settings_search, vm_storage as disk};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, VmStorageEdit};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, Synthetic};

type Window = Harness<UiView<Desktop>>;

fn window(scenario: Scenario, platform: &'static str) -> Window {
    podbag_desktop::icons::install();
    let (backend, events) = Backend::mock(
        MockOptions {
            scenario,
            platform: Some(platform),
            instant: true,
            ..MockOptions::default()
        },
        || {},
    );
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::new(UiView::new(app), 1200., 1250.);
    h.click_id(shell::nav_id(Page::Settings));
    h
}

fn drain(h: &mut Window) {
    h.message(Msg::Wake);
    h.repaint();
}

fn write(h: &mut Window, id: Id, text: &str) {
    h.click_id(id);
    h.press_key(KeyPress::shortcut('a'));
    h.input(Synthetic::Text(text.into()));
}

#[test]
fn stopped_vm_can_grow_and_move_the_disk_and_shows_the_recovery_path() {
    let mut h = window(Scenario::VmStopped, "macos");
    h.click_id(settings::section_id("vmStorage"));
    drain(&mut h);
    write(&mut h, disk::CAPACITY, "16");
    assert!(h.hitbox(disk::GROW).is_none());
    write(&mut h, disk::CAPACITY, "48");
    h.click_id(disk::GROW);
    drain(&mut h);
    assert!(h.shows("48 GiB"));
    assert!(h.shows("Applies on the next start"));
    assert!(h.shows("Recovery copy:"));
    write(&mut h, disk::PATH, "relative.img");
    assert!(h.hitbox(disk::MOVE).is_none());
    write(&mut h, disk::PATH, "/Volumes/Work/test.img");
    h.click_id(disk::MOVE);
    drain(&mut h);
    let app = &h.view().app;
    let status = app.api.vm_storage(app.session, None).unwrap();
    assert_eq!(status.path.to_string_lossy(), "/Volumes/Work/test.img");
    assert_eq!(status.size_bytes, 48 << 30);
    assert!(h.shows("Recovery copy: /Users/me/.podbag/vm/disk.img"));
}

#[test]
fn running_vm_shows_disk_details_and_refreshes_controls_after_stopping() {
    let mut h = window(Scenario::Default, "macos");
    h.click_id(settings::section_id("vmStorage"));
    drain(&mut h);
    assert!(h.shows("Stop the VM before changing its storage."));
    assert!(h.hitbox(disk::CAPACITY).is_none());
    let app = &h.view().app;
    assert!(
        app.api
            .vm_storage(app.session, Some(VmStorageEdit::Grow { gib: 96 }))
            .is_err()
    );
    app.api.vm("stop").unwrap();
    drain(&mut h);
    drain(&mut h);
    assert!(h.hitbox(disk::CAPACITY).is_some());
}

#[test]
fn storage_search_and_api_only_accept_a_local_mac_vm() {
    for (scenario, platform, shown) in [
        (Scenario::Default, "macos", true),
        (Scenario::Default, "linux", false),
        (Scenario::Default, "windows", false),
        (Scenario::Docker, "macos", false),
    ] {
        let mut h = window(scenario, platform);
        h.click_id(settings_search::SEARCH);
        h.input(Synthetic::Text("relocate".into()));
        assert_eq!(
            h.hitbox(settings_search::result_id("settings.vmStorage"))
                .is_some(),
            shown
        );
        let app = &h.view().app;
        assert_eq!(app.api.vm_storage(app.session, None).is_ok(), shown);
    }
}
