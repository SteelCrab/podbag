use podbag_desktop::views::{engine_config as config, settings, settings_search};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, ConfigEdit, EngineConfigRequest as Request};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{KeyPress, NamedKey, Point, Synthetic};

type Window = Harness<UiView<Desktop>>;

fn window(scenario: Scenario) -> Window {
    podbag_desktop::icons::install();
    let (backend, events) = Backend::mock(
        MockOptions {
            scenario,
            instant: true,
            ..MockOptions::default()
        },
        || {},
    );
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::with_fonts(UiView::new(app), 1200., 1100., podbag_ui::load_fonts());
    h.click_id(shell::nav_id(Page::Settings));
    h.click_id(settings::section_id("engine"));
    h
}

fn contents(h: &Window) -> Option<String> {
    let app = &h.view().app;
    app.api
        .engine_config(app.session, Request::Read)
        .unwrap()
        .unwrap()
        .text
}

fn set_saved(h: &Window, text: &str) {
    let app = &h.view().app;
    let edit = ConfigEdit {
        original: contents(h),
        text: text.into(),
    };
    app.api
        .engine_config(app.session, Request::Save { edit })
        .unwrap();
}

fn drain(h: &mut Window) {
    h.message(Msg::Wake);
    h.repaint();
}

#[test]
fn opens_validates_and_saves_multiline_text_with_existing_line_endings() {
    let mut h = window(Scenario::Default);
    set_saved(&h, "# original\r\n");
    h.click_id(config::OPEN);
    drain(&mut h);
    h.click_id(config::TEXT);
    h.press_key(KeyPress::shortcut('a'));
    h.clipboard()
        .write("# draft\nstore_limit = \"8 GiB\"".into());
    h.press_key(KeyPress::shortcut('v'));
    h.press_key(KeyPress::named(NamedKey::Enter));
    h.click_id(config::CHECK);
    drain(&mut h);
    assert!(h.shows("The configuration is valid."));
    assert_eq!(contents(&h).as_deref(), Some("# original\r\n"));
    h.click_id(config::SAVE);
    drain(&mut h);
    assert!(h.shows("Configuration saved."));
    assert_eq!(
        contents(&h).as_deref(),
        Some("# draft\r\nstore_limit = \"8 GiB\"\r\n")
    );
}

#[test]
fn pointer_and_arrow_keys_edit_the_selected_line_and_stale_saves_keep_the_draft() {
    let mut h = window(Scenario::Default);
    set_saved(&h, "# one\n# two\n# three");
    h.click_id(config::OPEN);
    drain(&mut h);
    let r = h.hitbox(config::TEXT).unwrap();
    h.click(Point::new(r.x + 11., r.y + 8. + 20. + 10.));
    h.press_key(KeyPress::named(NamedKey::End));
    h.input(Synthetic::Text("!".into()));
    h.press_key(KeyPress::named(NamedKey::ArrowDown));
    h.press_key(KeyPress::named(NamedKey::End));
    h.input(Synthetic::Text("?".into()));
    h.click_id(config::SAVE);
    drain(&mut h);
    assert_eq!(contents(&h).as_deref(), Some("# one\n# two!\n# three?"));
    set_saved(&h, "# another writer\n");
    h.click_id(config::TEXT);
    h.press_key(KeyPress::shortcut('a'));
    h.input(Synthetic::Text("invalid line".into()));
    h.click_id(config::CHECK);
    drain(&mut h);
    assert!(h.shows("expected `name = value`"));
    h.click_id(config::TEXT);
    h.press_key(KeyPress::shortcut('a'));
    h.input(Synthetic::Text("# keep draft".into()));
    h.click_id(config::SAVE);
    drain(&mut h);
    assert!(h.shows("config.toml changed after it was opened"));
    assert!(h.shows("# keep draft"));
    assert_eq!(contents(&h).as_deref(), Some("# another writer\n"));
    h.click_id(config::CLOSE);
    h.click_id(config::OPEN);
    drain(&mut h);
    assert!(h.shows("# another writer"));
}

#[test]
fn search_offers_the_editor_only_for_the_local_engine() {
    for (scenario, expected) in [(Scenario::Default, true), (Scenario::Docker, false)] {
        let mut h = window(scenario);
        h.click_id(settings_search::SEARCH);
        h.input(Synthetic::Text("config.toml".into()));
        let result = settings_search::result_id("engineConfig.title");
        assert_eq!(h.hitbox(result).is_some(), expected);
        if expected {
            h.click_id(result);
            assert!(h.hitbox(config::OPEN).is_some());
        } else {
            let app = &h.view().app;
            assert!(app.api.engine_config(app.session, Request::Read).is_err());
        }
    }
}
