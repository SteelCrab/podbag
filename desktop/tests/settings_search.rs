//! The search in Settings, on the mock backend: it finds a setting by its name, its
//! section and other words (in the language shown, accents or not), also in a closed
//! section; the arrows, Enter and Escape; a result opens its section and focuses the
//! setting; settings the page does not show are not found.

use podbag_desktop::views::settings;
use podbag_desktop::views::settings_search::{self, CLEAR, SEARCH, result_id};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{KeyPress, ModifiersState, NamedKey, Synthetic};

type H = Harness<UiView<Desktop>>;

fn window(opts: MockOptions) -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        ..opts
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::new(UiView::new(app), 1200., 900.);
    h.click_id(shell::nav_id(Page::Settings));
    h.message(Msg::Wake);
    h.repaint();
    h
}

fn english() -> H {
    window(MockOptions::default())
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

/// Type `text` into the search, replacing what it had.
fn query(h: &mut H, text: &str) {
    h.click_id(SEARCH);
    let m = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL
    };
    h.press_key(KeyPress::char("a").with_mods(m));
    h.press_key(KeyPress::named(NamedKey::Backspace));
    h.input(Synthetic::Text(text.into()));
}

fn press(h: &mut H, key: NamedKey) {
    h.press_key(KeyPress::named(key));
}

/// The keys of the results shown.
fn results(h: &H) -> Vec<&'static str> {
    let content = app(h).settings_search.input.content().to_string();
    settings_search::hits(app(h), &content)
        .into_iter()
        .map(|hit| hit.key)
        .filter(|key| h.hitbox(result_id(key)).is_some())
        .collect()
}

#[test]
fn a_setting_is_found_by_its_section_and_another_word() {
    let mut h = english();
    assert!(results(&h).is_empty(), "an empty search shows the settings");
    assert!(h.hitbox(settings::section_id("engine")).is_some());

    query(&mut h, "virtual machine RAM");
    assert_eq!(results(&h), ["settings.memory"]);
    assert!(h.shows("Memory (GB)"));
    assert!(h.shows("Virtual machine"), "the section's name shows below");
    assert!(
        h.hitbox(settings::section_id("engine")).is_none(),
        "the settings hide while the results show"
    );
    assert!(
        !app(&h).open_sections.contains("vm"),
        "a closed section is searched without opening it"
    );

    press(&mut h, NamedKey::ArrowDown);
    assert_eq!(h.focused(), Some(result_id("settings.memory")));
    press(&mut h, NamedKey::ArrowUp);
    assert_eq!(
        h.focused(),
        Some(SEARCH),
        "ArrowUp from the first result goes back to the search"
    );
    press(&mut h, NamedKey::Enter);
    assert!(
        app(&h).open_sections.contains("vm"),
        "Enter opens the setting's section"
    );
    assert_eq!(
        h.focused(),
        Some(app(&h).vm_form.memory.id),
        "the memory field has the focus"
    );
    assert_eq!(
        app(&h).settings_search.input.content(),
        "",
        "the search empties"
    );
    assert!(results(&h).is_empty());
}

#[test]
fn nothing_found_escape_and_clear() {
    let mut h = english();
    query(&mut h, "does not exist");
    assert!(h.shows("No settings found."));
    press(&mut h, NamedKey::Escape);
    assert_eq!(app(&h).settings_search.input.content(), "");
    assert!(!h.shows("No settings found."));
    assert_eq!(
        h.focused(),
        Some(SEARCH),
        "Escape keeps the focus in the search"
    );

    query(&mut h, "dark mode");
    assert_eq!(results(&h).first(), Some(&"settings.theme"));
    h.click_id(result_id("settings.theme"));
    assert_eq!(
        h.focused(),
        Some(settings::THEME),
        "the theme has the focus"
    );

    query(&mut h, "resource saver");
    assert_eq!(results(&h), ["settings.idle"]);
    h.click_id(CLEAR);
    assert_eq!(app(&h).settings_search.input.content(), "");
    assert_eq!(h.focused(), Some(SEARCH), "Clear gives the focus back");
}

#[test]
fn results_move_with_the_arrows() {
    let mut h = english();
    query(&mut h, "Virtual machine");
    let keys = results(&h);
    assert!(keys.len() > 2, "{keys:?}");
    press(&mut h, NamedKey::ArrowDown);
    press(&mut h, NamedKey::ArrowDown);
    assert_eq!(h.focused(), Some(result_id(keys[1])));
    press(&mut h, NamedKey::Enter);
    assert!(app(&h).open_sections.contains("vm"));
    assert_eq!(h.focused(), Some(app(&h).vm_form.cpus.id));
}

#[test]
fn other_languages_search_their_own_words() {
    let mut ko = window(MockOptions {
        language: Some("ko".into()),
        ..MockOptions::default()
    });
    assert!(ko.shows("설정 검색"), "the search is in Korean");
    query(&mut ko, "램");
    assert_eq!(results(&ko), ["settings.memory"]);
    assert!(ko.shows("메모리"));

    let mut es = window(MockOptions {
        language: Some("es".into()),
        ..MockOptions::default()
    });
    query(&mut es, "conexion");
    assert_eq!(
        results(&es).first(),
        Some(&"settings.engine"),
        "a word typed without its accent"
    );
    assert!(es.shows("Motor"));

    for lang in ["ja", "zh-CN", "vi", "fil"] {
        let mut h = window(MockOptions {
            language: Some(lang.into()),
            ..MockOptions::default()
        });
        let theme = app(&h).t("settings.theme");
        query(&mut h, &theme);
        assert_eq!(results(&h).first(), Some(&"settings.theme"), "{lang}");
    }
}

#[test]
fn settings_not_shown_are_not_found() {
    let mut docker = window(MockOptions {
        scenario: Scenario::Docker,
        ..MockOptions::default()
    });
    query(&mut docker, "compression");
    assert!(results(&docker).is_empty(), "compression is podbag's");

    let mut linux = window(MockOptions {
        platform: Some("linux"),
        ..MockOptions::default()
    });
    query(&mut linux, "RAM");
    assert!(results(&linux).is_empty(), "Linux has no VM");

    let mut windows = window(MockOptions {
        platform: Some("windows"),
        ..MockOptions::default()
    });
    query(&mut windows, "RAM");
    assert!(
        results(&windows).is_empty(),
        "WSL's resources are not set here"
    );
    query(&mut windows, "WSL");
    assert_eq!(results(&windows), ["settings.vm"]);
    query(&mut windows, "synchronized");
    assert!(
        results(&windows).is_empty(),
        "synchronized shares are the Mac's"
    );

    let mut mac = english();
    query(&mut mac, "sync conflicts");
    assert_eq!(results(&mac), ["settings.syncShares"]);
    query(&mut mac, "shared folders");
    assert_eq!(results(&mac).first(), Some(&"settings.sharedFolders"));
    press(&mut mac, NamedKey::Enter);
    assert_eq!(mac.focused(), Some(app(&mac).vm_form.share.id));
}
