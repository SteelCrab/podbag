//! Migrate: what the local engines or a Kubernetes cluster have, by kind,
//! to copy elsewhere; the dialog that copies one and follows its steps; and the
//! sidebar's sub-navigation with the counts. It is the start page.

use std::collections::BTreeMap;

use podbag_desktop_core::List;
use podbag_desktop_core::kubernetes::Resource;
use podbag_desktop_core::migration::Request;
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::dialog::{buttons_row, dialog};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::widgets::select::{Item as SelectItem, select};
use podbag_ui::widgets::tabs::tabs;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{CursorIcon, Id, Rect, TextInput, icons};

use crate::app::{Desktop, Page};
use crate::dialogs::{self, Dialog};
use crate::icons::Icon;
use crate::lists::{container_name, short_id};
use crate::views::blocks::{input, small, spawn};
use crate::views::training::toggle;
use crate::views::{TOOLBAR_H, title_bar};

pub const ENGINE_KINDS: [&str; 3] = ["containers", "images", "volumes"];
pub const KUBE_KINDS: [&str; 4] = ["pods", "services", "deployments", "claims"];

pub const REFRESH: Id = Id(0x6d69_6772_6101);
pub const TABS: Id = Id(0x6d69_6772_6102);
pub const SOURCE: Id = Id(0x6d69_6772_6103);
pub const SOCKET: Id = Id(0x6d69_6772_6104);
pub const NAMESPACE: Id = Id(0x6d69_6772_6105);
pub const TOGGLE: Id = Id(0x6d69_6772_6106);
pub const PROBLEMS: Id = Id(0x6d69_6772_6107);
pub const TARGET: Id = Id(0x6d69_6772_6108);
pub const CUSTOM: Id = Id(0x6d69_6772_6109);
pub const DEST_NAMESPACE: Id = Id(0x6d69_6772_610a);
pub const DEFAULTS: Id = Id(0x6d69_6772_610b);
pub const START: Id = Id(0x6d69_6772_610c);

/// A sidebar item: `root`, `engine`, `kube`, or a kind.
pub fn nav_id(key: &str) -> Id {
    Id::of(("mnav", key))
}

/// A resource's row (by kind, source and name).
pub fn row_id(kind: &str, source: &str, name: &str) -> Id {
    Id::of(("migration-row", kind, source, name))
}

/// Something to copy.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub kind: String,
    pub name: String,
    pub label: String,
    pub uid: String,
    pub namespace: String,
    pub source: String,
    pub source_label: String,
}

pub struct MigrateState {
    /// Kubernetes (true) or the engines.
    pub cluster: bool,
    /// The engine to read ("": all local ones; "custom": the socket).
    pub engine: String,
    pub context: String,
    pub namespace: String,
    /// The kind shown ("": all).
    pub kind: String,
    pub expanded: bool,
    pub counts: BTreeMap<String, usize>,
    pub items: Vec<Item>,
    pub problems: Vec<String>,
    pub pending: usize,
    revision: u64,
    /// The engines or contexts to pick: (value, label).
    pub choices: Vec<(String, String)>,
    pub picker_ready: bool,
    pub socket: TextInput,
    pub problems_open: bool,
    pub scroll: ScrollState,
}

impl MigrateState {
    pub fn new(socket_label: &str) -> MigrateState {
        MigrateState {
            cluster: false,
            engine: String::new(),
            context: String::new(),
            namespace: String::new(),
            kind: String::new(),
            expanded: true,
            counts: BTreeMap::new(),
            items: Vec::new(),
            problems: Vec::new(),
            pending: 0,
            revision: 0,
            choices: Vec::new(),
            picker_ready: false,
            socket: input(SOCKET, socket_label),
            problems_open: false,
            scroll: ScrollState::default(),
        }
    }

    fn kinds(&self) -> &'static [&'static str] {
        if self.cluster {
            &KUBE_KINDS
        } else {
            &ENGINE_KINDS
        }
    }

    fn visible(&self) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|i| !self.cluster || self.namespace.is_empty() || i.namespace == self.namespace)
            .collect()
    }

    /// The counts, and the kind shown only while it has rows.
    fn count(&mut self) {
        let mut counts = BTreeMap::new();
        for i in self.visible() {
            *counts.entry(i.kind.clone()).or_insert(0) += 1;
        }
        if self.pending == 0 && self.problems.is_empty() {
            for k in self.kinds() {
                counts.entry(k.to_string()).or_insert(0);
            }
        }
        self.counts = counts;
        if self.pending == 0 && self.counts.get(&self.kind).copied().unwrap_or(0) == 0 {
            self.kind.clear();
        }
        if self.pending == 0
            && self.cluster
            && !self.namespace.is_empty()
            && !self.items.iter().any(|i| i.namespace == self.namespace)
        {
            self.namespace.clear();
            self.count();
        }
    }
}

fn kind_label(d: &Desktop, cluster: bool, kind: &str) -> String {
    d.t(&format!("{}{kind}", if cluster { "kube." } else { "nav." }))
}

fn kind_icon(cluster: bool, kind: &str) -> Icon {
    match (cluster, kind) {
        (true, "pods") => Icon::KubePods,
        (true, "services") => Icon::KubeServices,
        (true, "deployments") => Icon::KubeDeployments,
        (true, _) => Icon::KubeClaims,
        (false, "containers") => Icon::Containers,
        (false, "images") => Icon::Images,
        _ => Icon::Volumes,
    }
}

fn resource(kind: &str) -> Resource {
    match kind {
        "pods" => Resource::Pods,
        "services" => Resource::Services,
        "deployments" => Resource::Deployments,
        _ => Resource::Claims,
    }
}

/// The rows of a list read from `source`, as Migrate shows them.
fn rows(list: List) -> Vec<(String, String, String)> {
    match list {
        List::Images(images) => images
            .into_iter()
            .flat_map(|i| {
                let tags: Vec<String> = i
                    .repo_tags
                    .iter()
                    .filter(|t| *t != "<none>:<none>")
                    .cloned()
                    .collect();
                let names = if tags.is_empty() {
                    vec![i.id.clone()]
                } else {
                    tags
                };
                names
                    .into_iter()
                    .map(|name| {
                        let label = if name == i.id {
                            short_id(&name).to_string()
                        } else {
                            name.clone()
                        };
                        (name, label, i.id.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
        List::Containers(cs) => cs
            .iter()
            .map(|c| (c.id.clone(), container_name(c), String::new()))
            .collect(),
        List::Volumes(vs) => vs
            .into_iter()
            .map(|v| (v.name.clone(), v.name, String::new()))
            .collect(),
        List::Networks(ns) => ns
            .into_iter()
            .map(|n| (n.name.clone(), n.name, String::new()))
            .collect(),
    }
}

/// Show the engines or the cluster, all kinds or one.
pub fn open(d: &mut Desktop, cluster: bool, kind: &str) {
    let m = &mut d.migrate;
    let changed = cluster != m.cluster;
    m.cluster = cluster;
    m.kind = kind.to_string();
    m.expanded = true;
    if changed {
        m.counts.clear();
    }
    if d.page != Page::Migrate {
        d.go(Page::Migrate);
    } else if changed {
        reload(d);
    }
}

fn choices_of(d: &Desktop, contexts: Vec<String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = [
        ("podbag", "Podbag"),
        ("docker", "Docker"),
        ("podman", "Podman"),
        ("kiri", "Kiri"),
    ]
    .iter()
    .map(|(v, l)| (v.to_string(), l.to_string()))
    .collect();
    out.extend(contexts.into_iter().map(|n| (format!("context:{n}"), n)));
    if !cfg!(windows) {
        out.push(("custom".into(), d.t("engine.socket")));
    }
    out
}

/// Read the sources, then each (source, kind) on its own.
pub fn reload(d: &mut Desktop) {
    let m = &mut d.migrate;
    m.revision += 1;
    m.items.clear();
    m.problems.clear();
    m.pending = 1;
    m.picker_ready = false;
    m.count();
    let revision = m.revision;
    let cluster = m.cluster;
    let (wanted, engine, socket) = (
        m.context.clone(),
        m.engine.clone(),
        m.socket.content().trim().to_string(),
    );
    let kube_now = d.kube.context.clone().unwrap_or_default();
    spawn(
        d,
        move |api| {
            if cluster {
                api.kube_contexts().map(|c| (c, Vec::new()))
            } else {
                Ok((api.contexts(), api.migration_sources()))
            }
        },
        move |d, r| {
            if d.migrate.revision != revision {
                return;
            }
            let (names, local) = match r {
                Ok(v) => v,
                Err(e) => {
                    let m = &mut d.migrate;
                    m.problems.push(e);
                    m.pending = 0;
                    m.picker_ready = true;
                    m.count();
                    return;
                }
            };
            let sources: Vec<String> = if cluster {
                let m = &mut d.migrate;
                m.choices = names.iter().map(|n| (n.clone(), n.clone())).collect();
                if !names.contains(&wanted) {
                    m.context = if names.contains(&kube_now) {
                        kube_now
                    } else {
                        names.first().cloned().unwrap_or_default()
                    };
                }
                if m.context.is_empty() {
                    Vec::new()
                } else {
                    vec![m.context.clone()]
                }
            } else {
                d.migrate.choices = choices_of(d, names);
                match engine.as_str() {
                    "custom" => [socket].into_iter().filter(|s| !s.is_empty()).collect(),
                    "" => local,
                    one => vec![one.to_string()],
                }
            };
            let kinds = d.migrate.kinds();
            let jobs: Vec<(String, &'static str)> = sources
                .iter()
                .flat_map(|s| kinds.iter().map(move |k| (s.clone(), *k)))
                .collect();
            let m = &mut d.migrate;
            m.picker_ready = true;
            m.pending = jobs.len();
            m.count();
            for (source, kind) in jobs {
                read(d, revision, cluster, source, kind);
            }
        },
    );
}

/// One kind of one source.
fn read(d: &mut Desktop, revision: u64, cluster: bool, source: String, kind: &'static str) {
    let src = source.clone();
    spawn(
        d,
        move |api| -> Result<Vec<(String, String, String, String)>, String> {
            if cluster {
                let snap = api.kube_list(resource(kind), true, Some(&src))?;
                return Ok(snap
                    .items
                    .iter()
                    .filter(|i| {
                        let m = &i["metadata"];
                        m["deletionTimestamp"].is_null()
                            && m["ownerReferences"].as_array().is_none_or(|a| a.is_empty())
                    })
                    .map(|i| {
                        let m = &i["metadata"];
                        let name = m["name"].as_str().unwrap_or_default().to_string();
                        (
                            name.clone(),
                            name,
                            m["uid"].as_str().unwrap_or_default().to_string(),
                            m["namespace"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect());
            }
            let kind = podbag_desktop_core::parse_kind(kind)?;
            let list = api.migration_list(&src, kind)?;
            Ok(rows(list)
                .into_iter()
                .map(|(n, l, u)| (n, l, u, String::new()))
                .collect())
        },
        move |d, r| {
            if d.migrate.revision != revision {
                return;
            }
            let label = d
                .migrate
                .choices
                .iter()
                .find(|(v, _)| *v == source)
                .map_or(source.clone(), |(_, l)| l.clone());
            match r {
                Ok(rows) => {
                    let items = rows.into_iter().map(|(name, label_, uid, namespace)| Item {
                        kind: kind.to_string(),
                        name,
                        label: label_,
                        uid,
                        namespace,
                        source: source.clone(),
                        source_label: label.clone(),
                    });
                    d.migrate.items.extend(items);
                }
                Err(e) => {
                    let what = kind_label(d, cluster, kind);
                    d.migrate.problems.push(format!("{source} · {what} · {e}"));
                }
            }
            let m = &mut d.migrate;
            m.pending = m.pending.saturating_sub(1);
            m.count();
        },
    );
}

// ---- the page ----

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let t = ui.theme;
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    title_bar(ui, bar, &d.t("migration.title"));
    let label = d.t("action.refresh");
    if button::icon_button(
        ui,
        REFRESH,
        Rect::new(bar.right() - 50., bar.y + 16., 30., 30.),
        Icon::Refresh.id(),
        &label,
        IconKind::Plain,
        true,
    ) {
        reload(d);
    }
    // The tabs hide while the sidebar shows the same choice.
    let tabs_shown = d.collapsed() || !d.migrate.expanded;
    if tabs_shown {
        let row = r.cut_top(44.);
        ui.p.border_bottom(row, t.line);
        let labels = [d.t("settings.engine"), d.t("nav.kubernetes")];
        let chosen = usize::from(d.migrate.cluster);
        if let Some(i) = tabs(ui, TABS, row.inset(12., 0., 12., 0.), &labels, chosen) {
            open(d, i == 1, "");
        }
    }
    let mut scroll = std::mem::take(&mut d.migrate.scroll);
    scroll_view(
        ui,
        Id::of("migration-scroll"),
        r,
        &mut scroll,
        |ui, area| {
            let x = area.x + 20.;
            let w = area.w - 40.;
            let mut y = area.y + 16.;
            y += filters(d, ui, x, y, w) + 14.;
            y += groups(d, ui, x, y, w);
            y += status(d, ui, x, y + 12., w) + 36.;
            y - area.y
        },
    );
    d.migrate.scroll = scroll;
}

/// The source (engine or context), the socket, and the namespace.
fn filters(d: &mut Desktop, ui: &mut Ui, x: f32, y: f32, w: f32) -> f32 {
    let m = &d.migrate;
    let cluster = m.cluster;
    let mut values: Vec<(String, String)> = Vec::new();
    if !cluster {
        values.push((String::new(), d.t("migration.allEngines")));
    }
    values.extend(m.choices.iter().cloned());
    let current = if cluster { &m.context } else { &m.engine };
    let sel = values.iter().position(|(v, _)| v == current).unwrap_or(0);
    let items: Vec<SelectItem> = values
        .iter()
        .map(|(_, l)| SelectItem::Option(l.as_str()))
        .collect();
    let mut x0 = x;
    let sw = 220f32.min(w);
    let enabled = m.picker_ready && !values.is_empty();
    let picked = select(ui, SOURCE, Rect::new(x0, y, sw, 34.), &items, sel, enabled);
    ui.tooltip(
        SOURCE.with("tip"),
        Rect::new(x0, y, sw, 34.),
        &d.t("migration.from"),
    );
    x0 += sw + 8.;
    if let Some(i) = picked {
        let value = values[i].0.clone();
        let m = &mut d.migrate;
        if cluster {
            m.context = value;
            m.namespace.clear();
        } else {
            m.engine = value;
        }
        reload(d);
        return 34.;
    }
    if !cluster && d.migrate.engine == "custom" {
        let fw = 280f32.min(x + w - x0);
        text_field(
            ui,
            &mut d.migrate.socket,
            Rect::new(x0, y, fw, 34.),
            None,
            true,
        );
        if ui.has_focus(SOCKET) && ui.key_pressed(NamedKey::Enter).is_some() {
            reload(d);
        }
        x0 += fw + 8.;
    }
    if cluster {
        let mut names: Vec<String> = d
            .migrate
            .items
            .iter()
            .map(|i| i.namespace.clone())
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        names.dedup();
        let all = d.t("kube.allNamespaces");
        let mut items = vec![SelectItem::Option(all.as_str())];
        items.extend(names.iter().map(|n| SelectItem::Option(n.as_str())));
        let sel = names
            .iter()
            .position(|n| *n == d.migrate.namespace)
            .map_or(0, |i| i + 1);
        let nw = 200f32.min(x + w - x0).max(80.);
        if let Some(i) = select(ui, NAMESPACE, Rect::new(x0, y, nw, 34.), &items, sel, true) {
            d.migrate.namespace = if i == 0 {
                String::new()
            } else {
                names[i - 1].clone()
            };
            d.migrate.count();
        }
    }
    34.
}

/// The resources by kind; returns the height.
fn groups(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let m = &d.migrate;
    let cluster = m.cluster;
    let mut y = y0;
    let mut clicked = None;
    let visible: Vec<Item> = m.visible().into_iter().cloned().collect();
    let shown_kind = m.kind.clone();
    let namespace_filter = !m.namespace.is_empty();
    for kind in m.kinds() {
        if !shown_kind.is_empty() && shown_kind != *kind {
            continue;
        }
        let mut rows: Vec<&Item> = visible.iter().filter(|i| i.kind == *kind).collect();
        if rows.is_empty() {
            continue;
        }
        rows.sort_by(|a, b| a.label.cmp(&b.label).then(a.source.cmp(&b.source)));
        if y > y0 {
            y += 20.;
        }
        // The heading: icon, kind and count.
        ui.p.icon(
            kind_icon(cluster, kind).id(),
            Rect::new(x, y + 2., 16., 16.),
            t.ink2,
        );
        let style = ui.style().with_size(14.).weight(700);
        let label = kind_label(d, cluster, kind);
        let l = ui.p.layout(&label, style, None);
        ui.p.draw_text(&l, x + 24., y, t.ink);
        let count = rows.len().to_string();
        let cs = ui.style().with_size(12.);
        let cl = ui.p.layout(&count, cs, None);
        ui.p.draw_text(&cl, x + 32. + l.width, y + 2., t.muted);
        y += 26.;
        let tip = d.t("migration.review");
        for item in rows {
            let id = row_id(kind, &item.source, &item.name);
            let r = Rect::new(x, y, w, 38.);
            y += 40.;
            let hovered = ui.hovered(id) || (ui.has_focus(id) && ui.focus_visible());
            if hovered {
                ui.p.rounded(r, 8., t.accent_soft);
            }
            let origin = if cluster {
                if namespace_filter {
                    String::new()
                } else {
                    item.namespace.clone()
                }
            } else {
                item.source_label.clone()
            };
            let os = ui.style().with_size(12.);
            let ol = ui.p.layout(&origin, os, Some((w * 0.26).max(40.)));
            ui.p.draw_text(
                &ol,
                r.right() - 36. - ol.width,
                r.y + (r.h - ol.height) / 2.,
                t.ink2,
            );
            ui.label(
                Rect::new(r.x + 12., r.y, r.w - 60. - ol.width, r.h),
                &item.label,
                ui.style(),
                t.ink,
            );
            ui.p.icon(
                Icon::Forward.id(),
                Rect::new(r.right() - 26., r.y + 12., 14., 14.),
                t.muted,
            );
            ui.p.hitbox(r, id, CursorIcon::Pointer);
            ui.focusable(id, FocusKind::Activate, r, 8.);
            ui.tooltip(id, r, &tip);
            if ui.clicked(id) {
                clicked = Some(item.clone());
            }
        }
    }
    if let Some(item) = clicked {
        confirm(d, item);
    }
    y - y0
}

/// The line under the lists: checking, nothing found, or what could not be read.
fn status(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let m = &d.migrate;
    let mut y = y0;
    if m.pending > 0 {
        y += small(ui, x, y, w, &d.t("engine.checking"), t.muted) + 6.;
    } else if m.visible().is_empty() && m.problems.is_empty() {
        y += small(ui, x, y, w, &d.t("migration.empty"), t.muted) + 6.;
    }
    if !d.migrate.problems.is_empty() {
        let open = d.migrate.problems_open;
        let label = d.t("migration.unavailable");
        let style = ui.style().with_size(13.);
        let l = ui.p.layout(&label, style, None);
        let r = Rect::new(x, y, l.width + 22., 20.);
        let chevron = if open {
            icons::CHEVRON_DOWN
        } else {
            icons::CHEVRON_RIGHT
        };
        ui.p.icon(chevron, Rect::new(x, y + 4., 12., 12.), t.muted);
        ui.p.draw_text(&l, x + 18., y + (20. - l.height) / 2., t.muted);
        ui.p.hitbox(r, PROBLEMS, CursorIcon::Pointer);
        ui.focusable(PROBLEMS, FocusKind::Activate, r, 4.);
        if ui.clicked(PROBLEMS) {
            d.migrate.problems_open = !open;
        }
        y += 24.;
        if open {
            for p in d.migrate.problems.clone() {
                y += small(ui, x + 18., y, w - 18., &format!("• {p}"), t.ink2) + 4.;
            }
        }
    }
    y - y0
}

// ---- the sidebar ----

/// Migrate, and below it the engines and the
/// cluster with the kinds that have something. `y` moves down past them.
pub fn nav(d: &mut Desktop, ui: &mut Ui, inner: Rect, y: &mut f32, collapsed: bool) {
    let t = ui.theme;
    let m = &d.migrate;
    let on_page = d.page == Page::Migrate;
    let expanded = m.expanded;
    let selected = if m.cluster { "kube" } else { "engine" };
    let active = on_page.then(|| {
        if expanded {
            if m.kind.is_empty() {
                selected.to_string()
            } else {
                m.kind.clone()
            }
        } else {
            "root".to_string()
        }
    });
    let total = (!m.counts.is_empty()).then(|| m.counts.values().sum::<usize>());
    let counts = m.counts.clone();
    let cluster = m.cluster;
    let r = Rect::new(inner.x, *y, inner.w, 34.);
    *y += 36.;
    let open_r = if collapsed {
        r
    } else {
        r.inset(0., 0., 32., 0.)
    };
    let label = d.t("migration.title");
    if crate::shell::nav_item(
        ui,
        open_r,
        crate::shell::nav_id(Page::Migrate),
        Icon::Migration,
        &label,
        total,
        active.as_deref() == Some("root"),
        collapsed,
    ) {
        open(d, cluster, "");
    }
    if !collapsed {
        let tr = Rect::new(r.right() - 30., r.y + 2., 30., 30.);
        if toggle(ui, TOGGLE, tr, expanded, &d.t("migration.kind")) {
            d.migrate.expanded = !expanded;
        }
    }
    if !expanded {
        return;
    }
    let sub = if collapsed { 0. } else { 28. };
    let top = *y;
    for group in ["engine", "kube"] {
        let is_kube = group == "kube";
        let open_group = group == selected;
        let r = Rect::new(inner.x, *y, inner.w, 30.).inset(sub, 0., 0., 0.);
        *y += 32.;
        let label = d.t(if is_kube {
            "nav.kubernetes"
        } else {
            "settings.engine"
        });
        let icon = if is_kube {
            Icon::Kubernetes
        } else {
            Icon::Engine
        };
        if crate::shell::nav_item(
            ui,
            r,
            nav_id(group),
            icon,
            &label,
            if open_group { total } else { None },
            active.as_deref() == Some(group),
            collapsed,
        ) {
            open(d, is_kube, "");
        }
        if !open_group {
            continue;
        }
        let kinds: &[&str] = if is_kube { &KUBE_KINDS } else { &ENGINE_KINDS };
        let sub2 = if collapsed { 0. } else { 10. };
        for kind in kinds {
            let n = counts.get(*kind).copied().unwrap_or(0);
            if n == 0 {
                continue;
            }
            let r = Rect::new(inner.x, *y, inner.w, 30.).inset(sub + sub2, 0., 0., 0.);
            *y += 32.;
            let label = kind_label(d, is_kube, kind);
            if crate::shell::nav_item(
                ui,
                r,
                nav_id(kind),
                kind_icon(is_kube, kind),
                &label,
                Some(n),
                active.as_deref() == Some(*kind),
                collapsed,
            ) {
                open(d, is_kube, kind);
            }
        }
    }
    if !collapsed {
        ui.p.fill(Rect::new(inner.x + 18., top, 1., *y - top - 2.), t.line);
    }
}

// ---- the migration dialog ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Confirm,
    Running,
    Done,
    Failed,
}

/// Where to, then the copy and its result.
pub struct MigrationDialog {
    pub item: Item,
    /// Where it can go: (value, label), without the source.
    pub targets: Vec<(String, String)>,
    pub target: usize,
    pub custom: TextInput,
    pub namespace: TextInput,
    pub defaults: bool,
    pub phase: Phase,
    pub status: String,
    /// What the route shows once the copy started.
    pub destination: String,
}

/// The dialog for the `n`th resource shown (previews).
pub fn confirm_nth(d: &mut Desktop, n: usize) {
    let mut items: Vec<Item> = d.migrate.visible().into_iter().cloned().collect();
    items.sort_by(|a, b| {
        let at = |k: &str| d.migrate.kinds().iter().position(|x| *x == k);
        at(&a.kind)
            .cmp(&at(&b.kind))
            .then(a.label.cmp(&b.label))
            .then(a.source.cmp(&b.source))
    });
    if let Some(item) = items.into_iter().nth(n) {
        confirm(d, item);
    }
}

fn confirm(d: &mut Desktop, item: Item) {
    let targets: Vec<(String, String)> = d
        .migrate
        .choices
        .iter()
        .filter(|(v, _)| *v != item.source)
        .cloned()
        .collect();
    let mut namespace = input(DEST_NAMESPACE, "");
    namespace.set_content(&item.namespace);
    let dialog = MigrationDialog {
        targets,
        target: 0,
        custom: input(CUSTOM, "unix:///path/to/docker.sock"),
        namespace,
        defaults: false,
        phase: Phase::Confirm,
        status: d.t("migration.preserve"),
        destination: String::new(),
        item,
    };
    d.focus_next = Some(dialogs::CANCEL);
    d.dialog = Some(Dialog::Migration(Box::new(dialog)));
}

impl MigrationDialog {
    fn destination(&self) -> String {
        match self.targets.get(self.target) {
            Some((v, _)) if v == "custom" => self.custom.content().trim().to_string(),
            Some((v, _)) => v.clone(),
            None => String::new(),
        }
    }

    fn ready(&self) -> bool {
        let to = self.destination();
        !to.is_empty()
            && to != self.item.source
            && (self.item.namespace.is_empty() || !self.namespace.content().trim().is_empty())
    }
}

/// A step of the copy that is running.
pub fn progress(d: &mut Desktop, step: &str) {
    let text = d.t(&format!("migration.{step}"));
    if let Some(Dialog::Migration(m)) = &mut d.dialog
        && m.phase == Phase::Running
    {
        m.status = text;
    }
}

/// The copy ended.
pub fn finished(d: &mut Desktop, result: Result<(), String>) {
    let verified = d.t("migration.verified");
    if let Some(Dialog::Migration(m)) = &mut d.dialog
        && m.phase == Phase::Running
    {
        match result {
            Ok(()) => {
                m.phase = Phase::Done;
                m.status = verified;
            }
            Err(e) => {
                m.phase = Phase::Failed;
                m.status = e;
            }
        }
        d.focus_next = Some(dialogs::YES);
    }
}

fn start(d: &mut Desktop, m: &mut MigrationDialog) {
    let request = Request {
        source: m.item.source.clone(),
        destination: m.destination(),
        kind: m.item.kind.clone(),
        destination_defaults: m.defaults,
        name: m.item.name.clone(),
        uid: m.item.uid.clone(),
        source_namespace: m.item.namespace.clone(),
        destination_namespace: m.namespace.content().trim().to_string(),
    };
    m.destination = match m.targets.get(m.target) {
        Some((v, _)) if v == "custom" => request.destination.clone(),
        Some((_, label)) => label.clone(),
        None => String::new(),
    };
    m.phase = Phase::Running;
    m.status = d.t("migration.prepare");
    d.jobs.migrate(request);
}

/// Start the copy of the dialog that shows (previews).
pub fn start_shown(d: &mut Desktop) {
    if let Some(Dialog::Migration(mut m)) = d.dialog.take() {
        if m.phase == Phase::Confirm && m.ready() {
            start(d, &mut m);
        }
        d.dialog = Some(Dialog::Migration(m));
    }
}

/// Paints the dialog; true when it closes.
pub fn paint_dialog(d: &mut Desktop, ui: &mut Ui, m: &mut MigrationDialog) -> bool {
    let t = ui.theme;
    let tr = d.tr;
    let mut close = false;
    let mut go = false;
    let running = m.phase == Phase::Running;
    if running {
        ui.repaint();
    }
    let escape = dialog(ui, dialogs::ID, 420., |ui, r| {
        let mut y = r.y;
        // The mark: the phase's icon in a square.
        let (icon, color, title) = match m.phase {
            Phase::Confirm => (Icon::Migration, t.accent_text, "migration.confirm"),
            Phase::Running => (Icon::Refresh, t.accent_text, "migration.running"),
            Phase::Done => (Icon::Check, t.green, "migration.done"),
            Phase::Failed => (Icon::Warning, t.amber, "migration.failed"),
        };
        let mark = Rect::new(r.x, y, 36., 36.);
        ui.p.rounded(mark, 10., t.accent_soft);
        ui.p.icon(icon.id(), mark.center(21., 21.), color);
        y += 50.;
        y += crate::views::blocks::heading(ui, r.x, y, r.w, &tr.t(title), 16.) + 14.;
        // The route.
        let label_style = ui.style().with_size(13.);
        let lw = [
            "migration.resource",
            "migration.from",
            "migration.to",
            "migration.namespace",
        ]
        .iter()
        .map(|k| ui.p.layout(&tr.t(k), label_style, None).width)
        .fold(80f32, f32::max)
        .min(r.w * 0.4);
        let vx = r.x + lw + 16.;
        let vw = r.w - lw - 16.;
        let row = |ui: &mut Ui, y: &mut f32, key: &str, value: &str| {
            ui.p.label(Rect::new(r.x, *y, lw, 20.), &tr.t(key), label_style, t.ink2);
            let h = ui.paragraph(vx, *y, vw, value, ui.style(), t.ink);
            *y += h.max(20.) + 10.;
        };
        row(ui, &mut y, "migration.resource", &m.item.label);
        let from = [m.item.source_label.as_str(), m.item.namespace.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" / ");
        row(ui, &mut y, "migration.from", &from);
        if m.phase == Phase::Confirm {
            ui.p.label(
                Rect::new(r.x, y + 7., lw, 20.),
                &tr.t("migration.to"),
                label_style,
                t.ink2,
            );
            let items: Vec<SelectItem> = m
                .targets
                .iter()
                .map(|(_, l)| SelectItem::Option(l.as_str()))
                .collect();
            if let Some(i) = select(
                ui,
                TARGET,
                Rect::new(vx, y, vw, 34.),
                &items,
                m.target,
                !items.is_empty(),
            ) {
                m.target = i;
            }
            y += 42.;
            if m.targets.get(m.target).is_some_and(|(v, _)| v == "custom") {
                text_field(ui, &mut m.custom, Rect::new(vx, y, vw, 34.), None, true);
                y += 42.;
            }
            if !m.item.namespace.is_empty() {
                ui.p.label(
                    Rect::new(r.x, y + 7., lw, 20.),
                    &tr.t("migration.namespace"),
                    label_style,
                    t.ink2,
                );
                text_field(ui, &mut m.namespace, Rect::new(vx, y, vw, 34.), None, true);
                y += 42.;
            }
            if m.item.kind == "containers" {
                let label = tr.t("migration.defaults");
                let cw = checkbox::width(ui, &label).min(r.w);
                checkbox::checkbox(
                    ui,
                    DEFAULTS,
                    Rect::new(r.x, y, cw, 24.),
                    Some(&mut m.defaults),
                    &label,
                    true,
                );
                y += 32.;
            }
        } else {
            row(ui, &mut y, "migration.to", &m.destination.clone());
            if !m.item.namespace.is_empty() {
                let ns = m.namespace.content().trim().to_string();
                row(ui, &mut y, "migration.namespace", &ns);
            }
        }
        // The track moves while the copy runs.
        if running {
            let track = Rect::new(r.x, y + 2., r.w, 3.);
            ui.p.rounded(track, 2., t.line);
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0., |d| d.as_secs_f32() % 2.4);
            let f = if secs < 1.2 {
                secs / 1.2
            } else {
                2. - secs / 1.2
            };
            let seg = track.w * 0.32;
            ui.p.rounded(
                Rect::new(track.x + (track.w - seg) * f, track.y, seg, 3.),
                2.,
                t.accent,
            );
            y += 14.;
        }
        let color = if m.phase == Phase::Failed {
            t.red
        } else {
            t.ink2
        };
        y += small(ui, r.x, y, r.w, &m.status, color) + 16.;
        match m.phase {
            Phase::Confirm => {
                let (cancel, start_label) = (tr.t("action.cancel"), tr.t("migration.start"));
                let widths = [
                    button::width(ui, &start_label, Some(Icon::Migration.id()), false),
                    button::width(ui, &cancel, None, false),
                ];
                let rects = buttons_row(r.x, y, r.w, &widths);
                if button::button(
                    ui,
                    dialogs::CANCEL,
                    rects[1],
                    &cancel,
                    None,
                    ButtonKind::Normal,
                    true,
                ) {
                    close = true;
                }
                if button::button(
                    ui,
                    START,
                    rects[0],
                    &start_label,
                    Some(Icon::Migration.id()),
                    ButtonKind::Primary,
                    m.ready(),
                ) {
                    go = true;
                }
                y += 32.;
            }
            Phase::Running => {}
            Phase::Done | Phase::Failed => {
                let label = tr.t("migration.close");
                let w = button::width(ui, &label, None, false);
                let rects = buttons_row(r.x, y, r.w, &[w]);
                if button::button(
                    ui,
                    dialogs::YES,
                    rects[0],
                    &label,
                    None,
                    ButtonKind::Primary,
                    true,
                ) {
                    close = true;
                }
                y += 32.;
            }
        }
        y - r.y
    });
    if go {
        start(d, m);
    }
    if escape && !running {
        close = true;
    }
    if close && m.phase == Phase::Done {
        reload(d);
    }
    close
}
