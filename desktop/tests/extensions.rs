use podbag_desktop::{Desktop, Msg, Options, Page, dialogs, shell, views::extensions as ext};
use podbag_desktop_core::{Backend, mock::Options as MockOptions};
use podbag_ui::{Synthetic, testing::Harness, ui::UiView};

type Window = Harness<UiView<Desktop>>;

fn window() -> Window {
    podbag_desktop::icons::install();
    let (api, events) = Backend::mock(
        MockOptions {
            instant: true,
            ..Default::default()
        },
        || {},
    );
    let mut h = Harness::new(
        UiView::new(Desktop::new(api, events, Options::inline())),
        1200.,
        1200.,
    );
    h.click_id(shell::nav_id(Page::Extensions));
    drain(&mut h);
    h
}
fn drain(h: &mut Window) {
    h.message(Msg::Wake);
    h.repaint();
}

#[test]
fn installs_grants_revokes_and_removes_the_selected_manifest() {
    let mut h = window();
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/workspace-checklist.json");
    h.click_id(ext::PATH);
    h.input(Synthetic::Text(file.to_string_lossy().into()));
    h.click_id(ext::INSTALL);
    drain(&mut h);
    assert!(h.shows("Workspace checklist"));
    assert!(!h.shows("Before a build"));
    h.click_id(ext::ENABLE);
    drain(&mut h);
    assert!(h.shows("Before a build"));
    assert!(h.shows("Allow engine summary above"));
    h.click_id(ext::GRANT);
    drain(&mut h);
    assert!(h.shows("Engine summary"));
    assert!(!h.shows("Allow engine summary above"));
    h.click_id(ext::ENABLE);
    drain(&mut h);
    h.click_id(ext::ENABLE);
    drain(&mut h);
    assert!(h.shows("Allow engine summary above"));
    h.click_id(ext::REMOVE);
    h.click_id(dialogs::CANCEL);
    assert!(h.hitbox(ext::item("workspace.checklist")).is_some());
    h.click_id(ext::REMOVE);
    h.click_id(dialogs::YES);
    drain(&mut h);
    assert!(h.shows("Install a local Podbag extension manifest to get started."));
}

#[test]
fn invalid_file_displays_a_translated_error_and_keeps_the_store_empty() {
    let mut h = window();
    h.click_id(ext::PATH);
    h.input(Synthetic::Text("absent-manifest.json".into()));
    h.click_id(ext::INSTALL);
    drain(&mut h);
    assert!(h.shows("This file is not a valid Podbag extension manifest."));
    assert!(h.shows("Install a local Podbag extension manifest to get started."));
}
