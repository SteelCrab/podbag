use podbag_desktop::views::{builders, settings, settings_search};
use podbag_desktop::{Desktop, Msg, Options, Page, dialogs, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::Options as MockOptions;
use podbag_ui::Synthetic;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;

type Window = Harness<UiView<Desktop>>;
const NAME: &str = "local-builder";

fn window() -> Window {
    podbag_desktop::icons::install();
    let (backend, events) = Backend::mock(
        MockOptions {
            instant: true,
            ..MockOptions::default()
        },
        || {},
    );
    let mut h = Harness::new(
        UiView::new(Desktop::new(backend, events, Options::inline())),
        1200.,
        1240.,
    );
    h.click_id(shell::nav_id(Page::Settings));
    h.click_id(settings::section_id("builders"));
    drain(&mut h);
    h
}

fn drain(h: &mut Window) {
    for _ in 0..2 {
        h.message(Msg::Wake);
        h.repaint();
    }
}

#[test]
fn inspects_selects_starts_stops_and_removes_a_local_builder() {
    let mut h = window();
    h.click_id(builders::row(NAME));
    drain(&mut h);
    assert!(h.shows("linux/arm64"));
    h.click_id(builders::control(NAME, "select"));
    drain(&mut h);
    assert!(h.hitbox(builders::control(NAME, "select")).is_none());
    h.click_id(builders::control(NAME, "start"));
    drain(&mut h);
    h.click_id(builders::control(NAME, "stop"));
    drain(&mut h);
    assert!(h.hitbox(builders::control(NAME, "start")).is_some());
    h.click_id(builders::control(NAME, "remove"));
    assert!(h.shows("Its BuildKit container and cache will be deleted."));
    h.click_id(dialogs::CANCEL);
    assert!(h.hitbox(builders::row(NAME)).is_some());
    h.click_id(builders::control(NAME, "remove"));
    h.click_id(dialogs::YES);
    drain(&mut h);
    assert!(h.hitbox(builders::row(NAME)).is_none());
    h.click_id(builders::row("podbag"));
    drain(&mut h);
    assert!(h.hitbox(builders::control("podbag", "remove")).is_none());
}

#[test]
fn search_returns_the_builder_section_and_engine_switch_drops_old_actions() {
    let mut h = window();
    h.click_id(settings_search::SEARCH);
    h.input(Synthetic::Text("Buildx".into()));
    h.click_id(settings_search::result_id("settings.builders"));
    h.click_id(builders::row(NAME));
    drain(&mut h);
    let session = h.view().app.session;
    h.click_id(builders::control(NAME, "remove"));
    h.view_mut().app.switch_engine("docker").unwrap();
    drain(&mut h);
    let app = &h.view().app;
    assert!(
        app.api
            .builder_action(
                session,
                "podbag",
                NAME,
                podbag_desktop_core::builders::Action::Remove
            )
            .is_err()
    );
    assert!(
        app.api
            .builder_list(app.session)
            .unwrap()
            .builders
            .iter()
            .any(|b| b.name == NAME)
    );
}
