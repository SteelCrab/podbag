//! OpenShift Routes in the Kubernetes view, on the mock backend: only the `ocp` context
//! is OpenShift. Its Routes tab and sidebar item show, a Route's host opens its address,
//! and a plain cluster falls back to Pods.

use podbag_desktop::app::KubeKind;
use podbag_desktop::views::kubernetes::{self, route_link, route_link_id};
use podbag_desktop::{Desktop, Msg, Options, Page, dialogs, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Calls, Options as MockOptions};
use podbag_ui::load_fonts;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, NamedKey, Point};

type H = Harness<UiView<Desktop>>;

/// The window on Kubernetes (Pods), with fonts when there are some: a link is as wide
/// as its text.
fn window(calls: &Calls) -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        calls: calls.clone(),
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::with_fonts(UiView::new(app), 1200., 900., load_fonts());
    h.view_mut().app.go(Page::Kubernetes);
    wake(&mut h);
    h
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn wake(h: &mut H) {
    h.message(Msg::Wake);
    h.repaint();
}

/// Choose a context in the page's select: 0 is the current one, then the kubeconfig's.
fn select_context(h: &mut H, name: &str) {
    let n = app(h).kube.contexts.iter().position(|c| c == name).unwrap() + 1;
    let select = Id::of("kube-context");
    h.click_id(select);
    h.click_id(select.with(("item", n)));
    wake(h);
}

/// A resource tab (Routes is the last).
fn tab(kind: KubeKind) -> Id {
    let n = KubeKind::OPENSHIFT.iter().position(|k| *k == kind).unwrap();
    Id::of("kube-tabs").with(n)
}

#[test]
fn routes_show_only_on_openshift() {
    let calls = Calls::default();
    let mut h = window(&calls);
    // A plain cluster: no Routes tab and no Routes in the sidebar.
    assert_eq!(app(&h).kube.kind, KubeKind::Pods);
    assert!(!app(&h).kube.openshift);
    assert!(h.hitbox(tab(KubeKind::Routes)).is_none(), "no Routes tab");
    assert!(
        h.hitbox(shell::kube_nav_id(KubeKind::Routes)).is_none(),
        "no Routes in the sidebar"
    );
    assert!(!h.shows("Routes"));

    // The OpenShift context: the tab and the sidebar item appear.
    assert!(app(&h).kube.contexts.iter().any(|c| c == "ocp"));
    select_context(&mut h, "ocp");
    assert!(app(&h).kube.openshift);
    assert!(h.hitbox(tab(KubeKind::Routes)).is_some(), "the Routes tab");
    assert!(
        h.hitbox(shell::kube_nav_id(KubeKind::Routes)).is_some(),
        "the Routes sidebar item"
    );
    assert_eq!(
        app(&h).kube_counts.get("routes"),
        Some(&2),
        "the Routes count"
    );
    let total: usize = KubeKind::ALL
        .iter()
        .map(|k| app(&h).kube_counts[k.key()])
        .sum();
    assert_eq!(total, 4, "the total stays that of the four kinds");

    h.click_id(tab(KubeKind::Routes));
    wake(&mut h);
    assert_eq!(
        app(&h).kube.kind,
        KubeKind::Routes,
        "the Routes tab is chosen"
    );
    assert_eq!(calls.of("kube_list").last().unwrap(), "routes ocp");
    for header in ["Name", "Namespace", "Host", "Path", "Service", "TLS"] {
        assert!(h.shows(header), "the {header} column");
    }
    let items = &app(&h).kube.items;
    assert_eq!(items.len(), 2, "the Routes of the namespace");
    assert_eq!(
        kubernetes::cells(KubeKind::Routes, &items[0]),
        [
            "shop",
            "default",
            "shop.apps.ocp.example.com",
            "/cart",
            "frontend",
            "edge"
        ]
    );
    assert_eq!(
        kubernetes::cells(KubeKind::Routes, &items[1]),
        ["api", "default", "api.apps.ocp.example.com", "", "api", ""]
    );
    assert!(h.shows("shop.apps.ocp.example.com") && h.shows("—"));
    assert_eq!(
        route_link(&items[0]).as_deref(),
        Some("https://shop.apps.ocp.example.com/cart"),
        "https with the path"
    );
    assert_eq!(
        route_link(&items[1]).as_deref(),
        Some("http://api.apps.ocp.example.com"),
        "http without TLS"
    );

    // The host opens the Route's address, without the inspect dialog. Without fonts
    // the link has no width to click.
    if h.hitbox(route_link_id(0)).is_some_and(|r| r.w > 0.) {
        h.click_id(route_link_id(0));
        wake(&mut h);
        assert_eq!(
            calls.of("kube_open_route"),
            ["ocp default shop https://shop.apps.ocp.example.com/cart"]
        );
        assert!(app(&h).dialog.is_none(), "no inspect dialog");
        assert!(app(&h).toasts.is_empty(), "no error");
    } else {
        eprintln!("no fonts: the link click is skipped");
    }
    // The row elsewhere still opens the JSON.
    let row = h.hitbox(kubernetes::TABLE.with(("row", 1usize))).unwrap();
    h.click(Point::new(row.right() - 20., row.y + row.h / 2.));
    assert!(matches!(
        app(&h).dialog,
        Some(dialogs::Dialog::KubeObject { .. })
    ));
    h.press_key(KeyPress::named(NamedKey::Escape));

    // Back to a plain cluster from the Routes page: Pods, with no request for Routes.
    let before = calls.of("kube_list").len();
    select_context(&mut h, "dev");
    assert_eq!(
        app(&h).kube.kind,
        KubeKind::Pods,
        "the page falls back to Pods"
    );
    let after = calls.of("kube_list")[before..].to_vec();
    assert_eq!(
        after,
        ["pods dev"],
        "only the Pods of the plain cluster, once"
    );
    assert!(h.hitbox(tab(KubeKind::Routes)).is_none());
    assert!(h.hitbox(shell::kube_nav_id(KubeKind::Routes)).is_none());
    assert!(app(&h).kube.error.is_none(), "no error");
    assert!(h.shows("worker-1"), "the Pods list");
    // History back to the Routes page stays on Pods off OpenShift.
    h.click_id(shell::NAV_BACK);
    assert_eq!(app(&h).page, Page::Kubernetes);
    assert_eq!(app(&h).kube.kind, KubeKind::Pods);
}

#[test]
fn the_sidebar_item_opens_routes() {
    let calls = Calls::default();
    let mut h = window(&calls);
    h.view_mut().app.command("kube-context ocp");
    wake(&mut h);
    h.click_id(shell::kube_nav_id(KubeKind::Routes));
    wake(&mut h);
    assert_eq!(app(&h).kube.kind, KubeKind::Routes);
    assert_eq!(app(&h).kube.items.len(), 2);
    // All namespaces: the staging Route too.
    h.click_id(Id::of("kube-all"));
    wake(&mut h);
    assert_eq!(app(&h).kube.items.len(), 3);
    assert_eq!(app(&h).kube_counts.get("routes"), Some(&3));
}
