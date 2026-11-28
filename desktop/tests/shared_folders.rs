//! The folders a Mac shares with the VM, in Settings, on the mock backend: add an
//! absolute path (with spaces), refuse a relative one, remove one, and save them for the
//! next start while the running VM keeps the folders it has.

use podbag_desktop::dialogs::{self, Dialog};
use podbag_desktop::views::settings::{self, ADD_SHARE, APPLY, remove_share_id};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Calls, Options as MockOptions};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, NamedKey, Synthetic};

type H = Harness<UiView<Desktop>>;

/// Settings with the VM's section open.
fn window(calls: &Calls, platform: Option<&'static str>) -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        platform,
        calls: calls.clone(),
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::new(UiView::new(app), 1200., 1500.);
    h.click_id(shell::nav_id(Page::Settings));
    h.message(Msg::Wake);
    h.click_id(settings::section_id("vm"));
    h
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn field(h: &H) -> Id {
    app(h).vm_form.share.id
}

fn set(h: &mut H, text: &str) {
    let id = field(h);
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

fn shares(h: &H) -> Vec<String> {
    app(h).vm_form.shares.clone()
}

/// The VM's shared folders: (next start, in use).
fn vm_shares(h: &H) -> (Vec<String>, Option<Vec<String>>) {
    let s = app(h)
        .vm
        .as_ref()
        .and_then(|vm| vm.shared_paths.clone())
        .expect("shared folders");
    (s.next, s.in_use)
}

#[test]
fn add_remove_and_save_for_the_next_start() {
    let calls = Calls::default();
    let mut h = window(&calls, None);
    assert!(h.shows("Shared folders"));
    assert_eq!(shares(&h), ["/Users"], "the default share");
    assert!(h.hitbox(remove_share_id("/Users")).is_some());

    set(&mut h, "relative");
    h.click_id(ADD_SHARE);
    assert_eq!(
        app(&h).vm_form.share_error,
        "Enter an absolute folder path."
    );
    assert!(h.shows("Enter an absolute folder path."));
    assert_eq!(shares(&h), ["/Users"], "a relative path is refused");

    let folder = "/private/tmp/Shared Project";
    set(&mut h, folder);
    assert!(
        app(&h).vm_form.share_error.is_empty(),
        "typing clears the error"
    );
    h.press_key(KeyPress::named(NamedKey::Enter));
    assert_eq!(shares(&h), ["/Users", folder], "Enter adds it");
    assert_eq!(app(&h).vm_form.share.content(), "");
    assert_eq!(h.focused(), Some(field(&h)));
    assert!(h.shows(folder));
    // A folder already there is not added twice.
    set(&mut h, folder);
    h.click_id(ADD_SHARE);
    assert_eq!(shares(&h).len(), 2);

    h.click_id(remove_share_id("/Users"));
    assert_eq!(shares(&h), [folder], "the default share is removed");
    assert_eq!(h.focused(), Some(remove_share_id(folder)));

    // The VM runs: the app asks when to use the new folders.
    h.click_id(APPLY);
    assert!(matches!(app(&h).dialog, Some(Dialog::Choose { .. })));
    h.click_id(dialogs::LATER);
    h.message(Msg::Wake);
    h.repaint();
    assert_eq!(calls.of("vm_set"), [folder]);
    assert_eq!(
        vm_shares(&h),
        (vec![folder.to_string()], Some(vec!["/Users".to_string()])),
        "the running VM keeps its folders until it restarts"
    );
    assert!(h.shows("Saved"));
}

#[test]
fn restart_now_uses_the_new_folders() {
    let calls = Calls::default();
    let mut h = window(&calls, None);
    set(&mut h, "/Volumes/Projects");
    h.click_id(ADD_SHARE);
    h.click_id(APPLY);
    h.click_id(dialogs::YES);
    h.message(Msg::Wake);
    h.repaint();
    // Sorted, as the window sends them.
    assert_eq!(calls.of("vm_set"), ["/Users,/Volumes/Projects"]);
    let both = vec!["/Users".to_string(), "/Volumes/Projects".to_string()];
    assert_eq!(vm_shares(&h), (both.clone(), Some(both)));
}

#[test]
fn removing_every_folder_shares_none() {
    let calls = Calls::default();
    let mut h = window(&calls, None);
    h.click_id(remove_share_id("/Users"));
    assert!(shares(&h).is_empty());
    assert_eq!(h.focused(), Some(field(&h)), "the field gets the focus");
    h.click_id(APPLY);
    h.click_id(dialogs::LATER);
    h.message(Msg::Wake);
    assert_eq!(calls.of("vm_set"), [""], "an empty list, not no list");
}

#[test]
fn windows_has_no_shared_folders() {
    let calls = Calls::default();
    let h = window(&calls, Some("windows"));
    assert!(!h.shows("Shared folders"));
    assert!(h.shows(&app(&h).t("settings.vmWsl")));
}
