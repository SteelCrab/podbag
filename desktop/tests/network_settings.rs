//! Drive the local network form against the same mock backend used by the app preview.
use podbag_desktop::views::{network_settings as net, settings, settings_search};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, DnsFilter, NetworkSettings};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, Synthetic};

type Window = Harness<UiView<Desktop>>;

fn window(scenario: Scenario) -> Window {
    podbag_desktop::icons::install();
    let options = MockOptions {
        scenario,
        platform: Some("linux"),
        instant: true,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(options, || {});
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

fn saved(window: &Window) -> NetworkSettings {
    let app = &window.view().app;
    app.api.network_settings(app.session, None).unwrap()
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

#[test]
fn changes_network_defaults_without_replacing_the_active_bridge() {
    let mut window = window(Scenario::Default);
    let [(_, ipv4), (_, mode), (_, ipv6), (_, dns), (_, filter)] = net::FIELDS;
    window.click_id(settings::section_id("network"));
    drain(&mut window);
    assert!(window.shows("The initialized bridge uses 10.77.0.0/16."));
    assert!(window.hitbox(ipv6).is_none());
    assert!(window.hitbox(filter).is_none());

    write(&mut window, ipv4, "10.81.0.0/16");
    window.click_id(mode);
    window.click_id(mode.with(("item", 1usize)));
    write(&mut window, ipv6, "fd77:706f:6462::/64");
    window.click_id(dns);
    window.click_id(filter);
    window.click_id(filter.with(("item", 2usize)));
    window.click_id(net::SAVE);
    drain(&mut window);

    let current = saved(&window);
    assert_eq!(current.preferences.default_subnet, "10.81.0.0/16");
    assert_eq!(
        current.preferences.default_ipv6_subnet.as_deref(),
        Some("fd77:706f:6462::/64")
    );
    assert!(current.preferences.embedded_dns);
    assert_eq!(current.preferences.dns_filter, DnsFilter::Ipv6);
    assert_eq!(current.active_subnet.as_deref(), Some("10.77.0.0/16"));
    assert!(window.shows("Network settings saved."));
}

#[test]
fn stale_save_keeps_the_draft_until_an_explicit_reload() {
    let mut window = window(Scenario::Default);
    window.click_id(settings::section_id("network"));
    drain(&mut window);
    let ipv4 = net::FIELDS[0].1;
    write(&mut window, ipv4, "10.82.0.0/16");

    let current = saved(&window);
    let mut other = current.preferences;
    other.default_subnet = "10.83.0.0/16".into();
    let app = &window.view().app;
    app.api
        .network_settings(app.session, Some((&other, &current.revision)))
        .unwrap();

    window.click_id(net::SAVE);
    drain(&mut window);
    assert!(window.shows("Network settings changed after they were opened"));
    assert!(window.shows("10.82.0.0/16"));
    assert_eq!(saved(&window).preferences, other);
    window.click_id(net::RELOAD);
    drain(&mut window);
    assert!(window.shows("10.83.0.0/16"));
}

#[test]
fn network_settings_search_only_returns_local_controls() {
    let mut local = window(Scenario::Default);
    let key = "networkSettings.dnsFilter";
    local.click_id(settings_search::SEARCH);
    local.input(Synthetic::Text("DNS query filtering".into()));
    local.click_id(settings_search::result_id(key));
    drain(&mut local);
    assert!(local.view().app.open_sections.contains("network"));
    assert!(local.shows("DNS query filtering"));

    let mut docker = window(Scenario::Docker);
    docker.click_id(settings_search::SEARCH);
    docker.input(Synthetic::Text("DNS query filtering".into()));
    assert!(docker.hitbox(settings_search::result_id(key)).is_none());
    let app = &docker.view().app;
    assert!(app.api.network_settings(app.session, None).is_err());
}
