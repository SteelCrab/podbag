//! The icons of Migrate, on the mock backend: each row shows its kind's icon, the
//! registry of its image (a configured host names its provider; an untagged image has
//! none) and the engine it comes from; the source picker and the dialog show the engine
//! or cluster chosen, and the dialog keeps the row's icons.

use podbag_desktop::dialogs::Dialog;
use podbag_desktop::icons::Icon;
use podbag_desktop::images;
use podbag_desktop::views::migrate::{self, SOURCE, TARGET, kind_icon, row_id};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::Options as MockOptions;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, Image, Rect, layer};

type H = Harness<UiView<Desktop>>;

/// Migrate, with the GitLab of the user's own at registry.example.com.
fn window() -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let mut app = Desktop::new(backend, events, Options::inline());
    app.pull_hosts
        .push(("gitlab-self".into(), "registry.example.com".into()));
    let mut h = Harness::new(UiView::new(app), 1200., 2400.);
    h.click_id(shell::nav_id(Page::Migrate));
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

/// Choose `value` in the source picker.
fn source(h: &mut H, value: &str) {
    let m = &app(h).migrate;
    let offset = usize::from(!m.cluster);
    let n = m.choices.iter().position(|(v, _)| v == value).unwrap() + offset;
    h.click_id(SOURCE);
    h.click_id(SOURCE.with(("item", n)));
    wake(h);
}

fn inside(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right() <= outer.right() + 0.5
        && inner.bottom() <= outer.bottom() + 0.5
}

/// The icons drawn inside `r`.
fn icons_in(h: &H, r: Rect) -> Vec<u16> {
    let layers = h.scene().layers.iter();
    layers
        .flat_map(|l| l.icons.iter())
        .filter(|i| inside(r, i.rect))
        .map(|i| i.icon)
        .collect()
}

/// The images drawn inside `r`.
fn images_in(h: &H, r: Rect) -> Vec<Image> {
    let layers = h.scene().layers.iter();
    layers
        .flat_map(|l| l.images.iter())
        .filter(|i| inside(r, i.rect))
        .map(|i| i.image)
        .collect()
}

fn mark(id: &str) -> Image {
    images::registry_icon(id).expect("a mark").0
}

/// The strip left of a widget (where its mark goes).
fn left_of(h: &H, id: Id) -> Rect {
    let r = h.hitbox(id).expect("shown");
    Rect::new(r.x - 30., r.y, 30., r.h)
}

fn registry(h: &H, name: &str) -> Option<&'static str> {
    let item = app(h).migrate.items.iter().find(|i| i.name == name);
    item.expect("listed").registry.as_ref().map(|r| r.id)
}

#[test]
fn engine_rows_show_their_kind_registry_and_engine() {
    let mut h = window();
    source(&mut h, "docker");
    assert!(
        images_in(&h, left_of(&h, SOURCE)).contains(&mark("dockerhub")),
        "the picker shows Docker"
    );
    let items = app(&h).migrate.items.clone();
    assert!(!items.is_empty());
    for item in &items {
        let r = h
            .hitbox(row_id(&item.kind, &item.source, &item.name))
            .expect("a row");
        let icons = icons_in(&h, r);
        assert!(
            icons.contains(&kind_icon(false, &item.kind).id()),
            "{}: the kind's icon",
            item.name
        );
        let drawn = images_in(&h, r);
        assert!(
            drawn.contains(&mark("dockerhub")),
            "{}: from Docker",
            item.name
        );
        if let Some(reg) = &item.registry {
            assert!(drawn.contains(&mark(reg.id)), "{}: {}", item.name, reg.id);
        }
    }
    for (name, id) in [
        ("example/api:1.5", "dockerhub"),
        ("registry.gitlab.com/example/api:1.5", "gitlab"),
        ("ghcr.io/example/frontend:2.0", "github"),
        ("quay.io/example/proxy:0.8", "quay"),
        ("registry.example.com/team/worker:1.4.2", "gitlab-self"),
    ] {
        assert_eq!(registry(&h, name), Some(id), "{name}");
    }
    assert_eq!(
        registry(&h, "sha256:8ab61e38dd2ac135"),
        None,
        "an untagged image has no registry"
    );
    // A container's registry is its image's.
    let worker = items
        .iter()
        .find(|i| i.kind == "containers" && i.label == "worker")
        .expect("worker");
    assert_eq!(worker.registry.as_ref().map(|r| r.id), Some("gitlab-self"));
    assert_eq!(
        worker.registry.as_ref().map(|r| r.label.as_str()),
        Some("GitLab Self-Managed")
    );
}

#[test]
fn the_dialog_keeps_the_icons_and_shows_the_destination() {
    let mut h = window();
    source(&mut h, "docker");
    let name = "registry.gitlab.com/example/api:1.5";
    let row = row_id("images", "docker", name);
    h.click_id(row);
    let Some(Dialog::Migration(m)) = &app(&h).dialog else {
        panic!("the dialog opens");
    };
    let reg = m.item.registry.clone().expect("a registry");
    assert_eq!((reg.id, reg.label.as_str()), ("gitlab", "GitLab"));
    let modal = &h.scene().layers[layer::MODAL];
    let drawn: Vec<Image> = modal.images.iter().map(|i| i.image).collect();
    assert!(drawn.contains(&mark("gitlab")), "the GitLab mark stays");
    assert!(drawn.contains(&mark("dockerhub")), "from Docker");
    assert!(modal.icons.iter().any(|i| i.icon == Icon::Images.id()));
    // The first destination is Podbag: the logo.
    assert!(images_in(&h, left_of(&h, TARGET)).contains(&images::logo()));
    let podman = m.targets.iter().position(|(v, _)| v == "podman").unwrap();
    h.click_id(TARGET);
    h.click_id(TARGET.with(("item", podman)));
    assert!(
        icons_in(&h, left_of(&h, TARGET)).contains(&Icon::Engine.id()),
        "another engine shows the engine icon"
    );
    h.click_id(podbag_desktop::dialogs::CANCEL);
    assert!(app(&h).dialog.is_none());
}

#[test]
fn clusters_show_kubernetes_and_their_kinds() {
    let mut h = window();
    h.click_id(migrate::nav_id("kube"));
    wake(&mut h);
    assert!(app(&h).migrate.cluster);
    assert!(
        images_in(&h, left_of(&h, SOURCE)).contains(&mark("kubernetes")),
        "the picker shows Kubernetes"
    );
    let items = app(&h).migrate.items.clone();
    assert!(!items.is_empty());
    for item in &items {
        let r = h
            .hitbox(row_id(&item.kind, &item.source, &item.name))
            .expect("a row");
        assert!(
            icons_in(&h, r).contains(&kind_icon(true, &item.kind).id()),
            "{}: the kind's icon",
            item.name
        );
        assert!(item.registry.is_none());
    }
}
