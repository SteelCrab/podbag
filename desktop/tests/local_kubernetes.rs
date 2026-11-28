//! Exercise the native local cluster controls against the mock engine.

use podbag_desktop::views::{local_kubernetes as local, settings_search};
use podbag_desktop::{Desktop, Msg, Options, Page, dialogs};
use podbag_desktop_core::Backend;
use podbag_desktop_core::kubernetes::Provider;
use podbag_desktop_core::mock::{Calls, Options as MockOptions};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, Synthetic};

type Window = Harness<UiView<Desktop>>;

fn window(platform: &'static str, calls: &Calls, incomplete: bool) -> Window {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        platform: Some(platform),
        instant: true,
        calls: calls.clone(),
        local_kube_incomplete: incomplete,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    Harness::new(UiView::new(app), 1200., 1000.)
}

fn app(w: &Window) -> &Desktop {
    &w.view().app
}

fn settle(w: &mut Window) {
    w.message(Msg::Wake);
    w.repaint();
}

fn write(w: &mut Window, id: Id, text: &str) {
    w.click_id(id);
    let select = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL
    };
    w.press_key(KeyPress::char("a").with_mods(select));
    w.input(Synthetic::Text(text.into()));
}

#[test]
fn macos_lifecycle_uses_the_chosen_engine_and_maintains_contexts() {
    let calls = Calls::default();
    let mut w = window("macos", &calls, false);
    w.view_mut().app.go(Page::Kubernetes);
    settle(&mut w);
    w.click_id(local::OPEN);
    settle(&mut w);
    assert_eq!(app(&w).page, Page::Settings);
    assert!(app(&w).open_sections.contains(local::SECTION));
    w.click_id(local::PROVIDER);
    w.click_id(local::PROVIDER.with(("item", 1usize)));
    write(&mut w, local::VERSION, "v1.35.8");
    write(&mut w, local::NODES, "2");
    w.click_id(local::CREATE);
    settle(&mut w);

    let cluster = app(&w).local_kube.cluster().unwrap();
    assert_eq!(cluster.provider, Provider::PodbagVm);
    assert_eq!(cluster.nodes, 2);
    assert_eq!(cluster.version.as_deref(), Some("v1.35.8"));
    assert!(cluster.ready);
    let context = cluster.context.clone();
    assert_eq!(app(&w).kube.context.as_deref(), Some(context.as_str()));
    assert!(app(&w).kube.contexts.contains(&context));
    assert_eq!(calls.of("local_kube_create"), ["true 2 v1.35.8"]);
    assert!(w.shows("Podbag virtual machine"));
    assert!(w.shows("Cluster created"));

    w.view_mut().app.set_kube_context(Some("staging".into()));
    settle(&mut w);
    w.click_id(local::RESET);
    assert!(w.shows("Its workloads and stored data will be deleted."));
    w.click_id(dialogs::CANCEL);
    assert!(calls.of("local_kube_reset").is_empty());
    w.click_id(local::RESET);
    w.click_id(dialogs::YES);
    settle(&mut w);
    assert_eq!(calls.of("local_kube_reset"), [context.as_str()]);
    assert_eq!(app(&w).kube.context.as_deref(), Some("staging"));
    assert!(app(&w).local_kube.cluster().unwrap().ready);

    w.view_mut().app.set_kube_context(Some(context.clone()));
    settle(&mut w);
    w.click_id(local::REMOVE);
    w.click_id(dialogs::YES);
    settle(&mut w);
    assert_eq!(calls.of("local_kube_remove"), [context.as_str()]);
    assert!(app(&w).local_kube.cluster().is_none());
    assert!(app(&w).kube.context.is_none());
    assert!(!app(&w).kube.contexts.contains(&context));
    assert!(app(&w).kube.contexts.iter().any(|c| c == "staging"));
}

#[test]
fn settings_search_finds_windows_docker_fallback_and_rejects_zero_nodes() {
    let calls = Calls::default();
    let mut w = window("windows", &calls, false);
    w.view_mut().app.go(Page::Settings);
    settle(&mut w);
    w.click_id(settings_search::SEARCH);
    w.input(Synthetic::Text("Kubernetes".into()));
    w.click_id(settings_search::result_id("kube.localTitle"));
    settle(&mut w);
    assert!(app(&w).open_sections.contains(local::SECTION));
    assert!(w.hitbox(local::PROVIDER).is_none());
    assert!(w.shows("Use a separate local Docker engine running Linux containers."));
    write(&mut w, local::NODES, "0");
    assert!(w.hitbox(local::CREATE).is_none());
    write(&mut w, local::NODES, "1");
    w.click_id(local::CREATE);
    settle(&mut w);
    assert_eq!(
        app(&w).local_kube.cluster().unwrap().provider,
        Provider::Docker
    );
    assert_eq!(calls.of("local_kube_create"), ["false 1 -"]);
}

#[test]
fn failed_creation_shows_owned_incomplete_cluster_and_allows_reset() {
    let calls = Calls::default();
    let mut w = window("linux", &calls, true);
    w.view_mut().app.open_section(local::SECTION);
    settle(&mut w);
    w.click_id(local::CREATE);
    settle(&mut w);
    let cluster = app(&w).local_kube.cluster().unwrap();
    assert!(!cluster.ready);
    assert!(app(&w).kube.context.is_none());
    assert!(!app(&w).kube.contexts.contains(&cluster.context));
    assert!(w.shows("Setup incomplete"));
    assert!(w.shows("The local cluster did not reach readiness"));
    assert!(w.hitbox(local::CREATE).is_none());

    w.click_id(local::RESET);
    w.click_id(dialogs::YES);
    settle(&mut w);
    let cluster = app(&w).local_kube.cluster().unwrap();
    assert!(cluster.ready);
    assert!(app(&w).kube.contexts.contains(&cluster.context));
    assert!(w.shows("Cluster created"));
}
