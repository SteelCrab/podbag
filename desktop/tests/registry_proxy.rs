//! The registry proxy form, its supported engines and the engine's validation.
use podbag_desktop::views::{registry_proxy as proxy, settings, settings_search};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, RegistryProxyMode};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, Synthetic};

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
    let mut window = Harness::new(UiView::new(app), 1200., 1000.);
    window.click_id(shell::nav_id(Page::Settings));
    drain(&mut window);
    window
}

fn drain(window: &mut Window) {
    window.message(Msg::Wake);
    window.repaint();
}

fn write(window: &mut Window, id: Id, value: &str) {
    window.click_id(id);
    let modifier = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL
    };
    window.press_key(KeyPress::char("a").with_mods(modifier));
    window.input(Synthetic::Text(value.into()));
}

fn mode(window: &mut Window, index: usize) {
    window.click_id(proxy::MODE);
    window.click_id(proxy::MODE.with(("item", index)));
}

#[test]
fn saves_manual_proxy_and_clears_it_when_direct_mode_is_saved() {
    let mut window = window(Scenario::Default, "linux");
    let section = settings::section_id("registryProxy");
    window.click_id(section);
    drain(&mut window);
    assert!(window.shows("Use engine environment"));
    assert!(window.hitbox(proxy::URL).is_none());

    mode(&mut window, 2);
    assert!(window.hitbox(proxy::URL).is_some());
    write(&mut window, proxy::URL, "https://proxy.invalid:3128/path");
    write(&mut window, proxy::BYPASS, "localhost,  .example.test,");
    window.click_id(proxy::SAVE);
    drain(&mut window);
    assert!(window.shows("Could not save the registry proxy settings"));
    assert!(window.shows("https://proxy.invalid:3128/path"));
    let app = &window.view().app;
    assert_eq!(
        app.api.registry_proxy(app.session, None).unwrap().mode,
        RegistryProxyMode::Inherited,
    );

    write(&mut window, proxy::URL, "https://proxy.invalid:3128");
    window.click_id(proxy::SAVE);
    drain(&mut window);
    assert!(window.shows("Registry proxy settings saved."));
    let app = &window.view().app;
    let saved = app.api.registry_proxy(app.session, None).unwrap();
    assert_eq!(saved.mode, RegistryProxyMode::Manual);
    assert_eq!(saved.url, "https://proxy.invalid:3128");
    assert_eq!(saved.bypass, "localhost, .example.test");

    mode(&mut window, 1);
    assert!(window.hitbox(proxy::URL).is_none());
    window.click_id(proxy::SAVE);
    drain(&mut window);
    let app = &window.view().app;
    let saved = app.api.registry_proxy(app.session, None).unwrap();
    assert_eq!(saved.mode, RegistryProxyMode::Direct);
    assert!(saved.url.is_empty() && saved.bypass.is_empty());

    mode(&mut window, 0);
    window.click_id(proxy::SAVE);
    drain(&mut window);
    let app = &window.view().app;
    assert_eq!(
        app.api.registry_proxy(app.session, None).unwrap().mode,
        RegistryProxyMode::Inherited,
    );
    mode(&mut window, 2);
    assert!(window.hitbox(proxy::URL).is_some());
    assert!(!window.shows("https://proxy.invalid:3128"));
}

#[test]
fn search_opens_registry_proxy_for_supported_local_engines_only() {
    let mut mac = window(Scenario::Default, "macos");
    let section = settings::section_id("registryProxy");
    let result = settings_search::result_id("settings.registryProxy");
    mac.click_id(settings_search::SEARCH);
    mac.input(Synthetic::Text("NO_PROXY".into()));
    mac.click_id(result);
    drain(&mut mac);
    assert!(mac.hitbox(section).is_some());
    assert!(mac.hitbox(proxy::MODE).is_some());

    for (scenario, platform) in [(Scenario::Docker, "linux"), (Scenario::Default, "windows")] {
        let mut unsupported = window(scenario, platform);
        assert!(unsupported.hitbox(section).is_none());
        unsupported.click_id(settings_search::SEARCH);
        unsupported.input(Synthetic::Text("NO_PROXY".into()));
        assert!(unsupported.hitbox(result).is_none());
        let app = &unsupported.view().app;
        assert!(app.api.registry_proxy(app.session, None).is_err());
    }
}
