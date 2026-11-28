//! The Kubernetes page: resource tabs (Routes only on OpenShift), the all-namespaces
//! toggle, the cluster context, and a table of the chosen resource. A row opens its
//! JSON; a Route's host opens its address in the browser.

use podbag_desktop_core::kubernetes::route_url;
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::layout::Row;
use podbag_ui::widgets::logs::LogView;
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::table::{Column, Width, table};
use podbag_ui::widgets::tabs::tabs;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{Color, CursorIcon, Id, Rect, TextInput, TextStyle, VirtualList};
use serde_json::Value;

use crate::app::{Desktop, KubeKind, pretty};
use crate::dialogs::Dialog;
use crate::icons::Icon;
use crate::views::blocks::spawn;
use crate::views::{TOOLBAR_H, centered, title_bar};

pub const SEARCH: Id = Id(0x006b_7562_6501);
pub const TABLE: Id = Id(0x006b_7562_6502);

pub struct KubeState {
    pub kind: KubeKind,
    pub all_namespaces: bool,
    /// None: the current context.
    pub context: Option<String>,
    pub contexts: Vec<String>,
    pub search: TextInput,
    pub items: Vec<Value>,
    /// The context and namespace of the loaded list.
    pub shown_context: String,
    pub namespace: String,
    pub error: Option<String>,
    pub loading: bool,
    /// Results of older loads are dropped.
    pub generation: u64,
    pub table: VirtualList,
    /// The context serves OpenShift Routes.
    pub openshift: bool,
    pub openshift_generation: u64,
    /// Load the list when the OpenShift answer comes (the context changed on Routes).
    pub load_after_openshift: bool,
}

impl KubeState {
    pub fn new(placeholder: &str) -> Self {
        Self {
            kind: KubeKind::Pods,
            all_namespaces: false,
            context: None,
            contexts: Vec::new(),
            search: TextInput::new(
                SEARCH,
                placeholder,
                TextStyle::new(14.).line_height(20.),
                Color::TRANSPARENT,
            ),
            items: Vec::new(),
            shown_context: String::new(),
            namespace: String::new(),
            error: None,
            loading: false,
            generation: 0,
            table: VirtualList::new(44., 40.),
            openshift: false,
            openshift_generation: 0,
            load_after_openshift: false,
        }
    }
}

fn columns(kind: KubeKind) -> Vec<&'static str> {
    let mut v = vec!["col.name", "kube.namespace"];
    v.extend_from_slice(match kind {
        KubeKind::Pods => &["kube.phase", "kube.ready", "kube.node"][..],
        KubeKind::Services => &["kube.type", "kube.address", "col.ports"],
        KubeKind::Deployments => &["kube.ready", "col.image"],
        KubeKind::Claims => &["kube.phase", "kube.capacity", "kube.storageClass"],
        KubeKind::Routes => &["kube.host", "kube.path", "kube.service", "kube.tls"],
    });
    v
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// The cells of a resource's row.
pub fn cells(kind: KubeKind, item: &Value) -> Vec<String> {
    let (meta, spec, status) = (&item["metadata"], &item["spec"], &item["status"]);
    let mut out = vec![s(&meta["name"]), s(&meta["namespace"])];
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    match kind {
        KubeKind::Pods => {
            let sidecars = list(&spec["initContainers"])
                .into_iter()
                .filter(|c| c["restartPolicy"] == "Always");
            let containers: Vec<Value> = list(&spec["containers"])
                .into_iter()
                .chain(sidecars)
                .collect();
            let ready: Vec<String> = list(&status["containerStatuses"])
                .into_iter()
                .chain(list(&status["initContainerStatuses"]))
                .filter(|c| c["ready"] == true)
                .map(|c| s(&c["name"]))
                .collect();
            let count = containers
                .iter()
                .filter(|c| ready.contains(&s(&c["name"])))
                .count();
            out.extend([
                s(&status["phase"]),
                format!("{count}/{}", containers.len()),
                s(&spec["nodeName"]),
            ]);
        }
        KubeKind::Deployments => {
            let images: Vec<String> = list(&spec["template"]["spec"]["containers"])
                .iter()
                .map(|c| s(&c["image"]))
                .collect();
            let ready = status["readyReplicas"].as_i64().unwrap_or(0);
            let replicas = spec["replicas"].as_i64().unwrap_or(1);
            out.extend([format!("{ready}/{replicas}"), images.join(", ")]);
        }
        KubeKind::Claims => {
            out.extend([
                s(&status["phase"]),
                s(&status["capacity"]["storage"]),
                s(&spec["storageClassName"]),
            ]);
        }
        KubeKind::Routes => {
            out.extend([
                s(&spec["host"]),
                s(&spec["path"]),
                s(&spec["to"]["name"]),
                s(&spec["tls"]["termination"]),
            ]);
        }
        KubeKind::Services => {
            let ports: Vec<String> = list(&spec["ports"])
                .iter()
                .map(|p| {
                    let node = p["nodePort"]
                        .as_i64()
                        .filter(|n| *n != 0)
                        .map(|n| format!(":{n}"))
                        .unwrap_or_default();
                    let proto = p["protocol"].as_str().unwrap_or("TCP");
                    format!("{}{node}/{proto}", s(&p["port"]))
                })
                .collect();
            let ips: Vec<String> = list(&spec["clusterIPs"]).iter().map(s).collect();
            let address = spec["externalName"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if ips.is_empty() {
                        s(&spec["clusterIP"])
                    } else {
                        ips.join(", ")
                    }
                });
            out.extend([s(&spec["type"]), address, ports.join(", ")]);
        }
    }
    out
}

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let t = ui.theme;
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    let title = d.t("nav.kubernetes");
    let title_right = title_bar(ui, bar, &title);
    let mut row = Row::new(bar.inset(20., 14., 20., 15.), 8.);
    let refresh = row.right_h(30., 30.);
    let label = d.t("kube.localTitle");
    let width = button::width(ui, &label, None, false);
    let local = row.right_h(width, 32.);
    if button::button(
        ui,
        super::local_kubernetes::OPEN,
        local,
        &label,
        None,
        button::Kind::Normal,
        true,
    ) {
        let key = super::local_kubernetes::SECTION;
        d.open_section(key);
        d.focus_next = Some(super::settings::section_id(key));
    }
    let x = title_right + 12.;
    let w = (row.rest().right() - x - 12.).clamp(0., 280.);
    let ready = d.kube.error.is_none() && !d.kube.loading;
    if w >= 120. {
        text_field(
            ui,
            &mut d.kube.search,
            Rect::new(x, bar.y + 15., w, 32.),
            Some(Icon::Search.id()),
            ready,
        );
    }
    let label = d.t("action.refresh");
    if button::icon_button(
        ui,
        Id::of("kube-refresh"),
        refresh,
        Icon::Refresh.id(),
        &label,
        IconKind::Plain,
        true,
    ) {
        d.kube_reload();
    }
    // The resource tabs and the all-namespaces toggle.
    let tab_bar = r.cut_top(44.);
    ui.p.border_bottom(tab_bar, t.line);
    let kinds = d.kube_kinds();
    let labels: Vec<String> = kinds
        .iter()
        .map(|k| d.t(&format!("kube.{}", k.key())))
        .collect();
    let selected = kinds.iter().position(|k| *k == d.kube.kind).unwrap_or(0);
    if let Some(n) = tabs(
        ui,
        Id::of("kube-tabs"),
        tab_bar.inset(20., 0., 20., 0.),
        &labels,
        selected,
    ) {
        d.go_kube(kinds[n]);
    }
    let all_label = d.t("kube.allNamespaces");
    let cw = checkbox::width(ui, &all_label);
    let mut all = d.kube.all_namespaces;
    if checkbox::checkbox(
        ui,
        Id::of("kube-all"),
        Rect::new(tab_bar.right() - 20. - cw, tab_bar.y, cw, tab_bar.h),
        Some(&mut all),
        &all_label,
        true,
    ) {
        d.kube.all_namespaces = all;
        d.refresh_kube_counts();
        d.kube_load();
    }
    // The cluster context.
    r.cut_top(12.);
    let ctx_row = r.cut_top(34.).inset(20., 0., 20., 0.);
    let label = d.t("kube.cluster");
    let style = ui.style().with_size(13.);
    let lw = ui.p.layout(&label, style, None).width;
    ui.p.label(ctx_row.with_w(lw + 2.), &label, style, t.ink);
    let current = d.t("kube.current");
    let mut names: Vec<String> = d.kube.contexts.clone();
    if let Some(c) = &d.kube.context
        && !names.contains(c)
    {
        names.push(c.clone());
    }
    let mut items = vec![Item::Option(&current)];
    items.extend(names.iter().map(|n| Item::Option(n.as_str())));
    let sel = d
        .kube
        .context
        .as_ref()
        .and_then(|c| names.iter().position(|n| n == c))
        .map_or(0, |i| i + 1);
    let sw = 240f32.min(ctx_row.w - lw - 10.);
    if let Some(i) = select(
        ui,
        Id::of("kube-context"),
        Rect::new(ctx_row.x + lw + 10., ctx_row.y, sw, 34.),
        &items,
        sel,
        true,
    ) {
        let context = (i > 0).then(|| names[i - 1].clone());
        d.set_kube_context(context);
    }
    // The scope line.
    r.cut_top(10.);
    let line = r.cut_top(20.).inset(20., 0., 20., 0.);
    if d.kube.error.is_none() && !d.kube.shown_context.is_empty() {
        let ctx = d.tv("kube.context", &[("context", &d.kube.shown_context)]);
        let scope = if d.kube.all_namespaces {
            d.t("kube.allNamespaces")
        } else {
            d.kube.namespace.clone()
        };
        let text = format!("{ctx} · {scope}");
        ui.label(line, &text, ui.style().with_size(12.5), t.ink2);
    }
    let content = r.inset(20., 8., 20., 0.);
    if d.kube.loading {
        let text = d.t("engine.checking");
        ui.p.label(
            Rect::new(content.x + 10., content.y + 24., content.w, 20.),
            &text,
            ui.style(),
            t.ink2,
        );
        return;
    }
    if let Some(err) = d.kube.error.clone() {
        let message = if err == "kubectl-missing" {
            d.t("kube.missing")
        } else {
            err
        };
        let cx = content.x + content.w / 2.;
        let mut y = content.y + 48.;
        let h2 = ui.style().with_size(18.).weight(700);
        y += centered(ui, cx, y, content.w - 40., &d.t("kube.error"), h2, t.ink) + 8.;
        let mono = TextStyle::new(12.5).mono().line_height(18.);
        y += centered(ui, cx, y, content.w - 40., &message, mono, t.red) + 14.;
        let label = d.t("action.retry");
        let w = button::width(ui, &label, Some(Icon::Refresh.id()), false);
        if button::button(
            ui,
            Id::of("kube-retry"),
            Rect::new(cx - w / 2., y, w, 32.),
            &label,
            Some(Icon::Refresh.id()),
            button::Kind::Primary,
            true,
        ) {
            d.kube_reload();
        }
        return;
    }
    let kind = d.kube.kind;
    let q = d.kube.search.content().trim().to_lowercase();
    // The rows that match the search, with their index in `items`.
    let (item_ix, rows): (Vec<usize>, Vec<Vec<String>>) = d
        .kube
        .items
        .iter()
        .map(|item| cells(kind, item))
        .enumerate()
        .filter(|(_, v)| q.is_empty() || v.iter().any(|c| c.to_lowercase().contains(&q)))
        .unzip();
    if rows.is_empty() {
        let text = if d.kube.items.is_empty() {
            d.t(&format!("kube.empty.{}", kind.key()))
        } else {
            d.tv("empty.search", &[("q", d.kube.search.content().trim())])
        };
        ui.paragraph(
            content.x + 10.,
            content.y + 24.,
            content.w - 20.,
            &text,
            ui.style(),
            t.ink2,
        );
        return;
    }
    let keys = columns(kind);
    let labels: Vec<String> = keys.iter().map(|k| d.t(k)).collect();
    let cols: Vec<Column> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| Column {
            label: l,
            width: if i == 0 {
                Width::Flex(0.27 * (keys.len() - 1) as f32 / 0.73)
            } else {
                Width::Flex(1.)
            },
        })
        .collect();
    // A Route's host is a link to its address.
    let links: Vec<Option<String>> = item_ix
        .iter()
        .map(|&i| {
            (kind == KubeKind::Routes)
                .then(|| route_link(&d.kube.items[i]))
                .flatten()
        })
        .collect();
    let mut open_route = None;
    let base = ui.style();
    let mut table_state = std::mem::replace(&mut d.kube.table, VirtualList::new(44., 40.));
    let clicked = table(
        ui,
        TABLE,
        content,
        &cols,
        rows.len(),
        &mut table_state,
        |ui, row, col, cell| {
            let v = &rows[row][col];
            let text = if v.is_empty() { "—" } else { v.as_str() };
            if col == 2
                && let Some(url) = &links[row]
            {
                if link(ui, route_link_id(row), cell, text, url, base) {
                    open_route = Some(row);
                }
            } else if col == 0 {
                let style = base.weight(600);
                ui.label(cell, text, style, t.ink);
                let w = ui.p.layout(text, style, None).width.min(cell.w);
                ui.focusable(
                    TABLE.with(("row", row)),
                    FocusKind::Activate,
                    cell.center_v(20.).with_w(w),
                    4.,
                );
            } else {
                ui.label(cell, text, base, t.ink);
            }
        },
    );
    d.kube.table = table_state;
    if let Some(row) = open_route {
        let item = &d.kube.items[item_ix[row]];
        let (namespace, name) = (
            s(&item["metadata"]["namespace"]),
            s(&item["metadata"]["name"]),
        );
        let context = d.kube.shown_context.clone();
        spawn(
            d,
            move |api| api.kube_open_route(&context, &namespace, &name),
            |d, result| {
                if let Err(e) = result {
                    d.toasts.push(e, true);
                }
            },
        );
    } else if let Some(row) = clicked {
        let item = &d.kube.items[item_ix[row]];
        let title = format!(
            "{} / {}",
            s(&item["metadata"]["namespace"]),
            s(&item["metadata"]["name"])
        );
        let context = d.tv("kube.context", &[("context", &d.kube.shown_context)]);
        let mut lines = LogView::default();
        lines.follow = false;
        lines.set_text(&pretty(item));
        d.open_dialog(Dialog::KubeObject {
            title,
            context,
            lines,
        });
    }
}

/// The id of the host link in a Routes row.
pub fn route_link_id(row: usize) -> Id {
    TABLE.with(("route", row))
}

/// The address a Route publishes (https with TLS settings), if it has a valid host.
pub fn route_link(item: &Value) -> Option<String> {
    route_url(item).ok()
}

/// Text in the accent color, underlined under the pointer, with the address as its
/// tooltip. The app opens the address it reads from the cluster, not this one. True when
/// clicked.
fn link(ui: &mut Ui, id: Id, cell: Rect, text: &str, url: &str, style: TextStyle) -> bool {
    let t = ui.theme;
    let layout = ui.p.label(cell, text, style, t.accent_text);
    let w = layout.width.min(cell.w);
    let r = cell.center_v(style.line_height).with_w(w);
    if ui.hovered(id) {
        ui.p.fill(Rect::new(r.x, r.bottom() - 1., w, 1.), t.accent_text);
    }
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r, 4.);
    ui.tooltip(id, r, url);
    ui.clicked(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cells_read_like_the_web_page() {
        let pod = json!({
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"containers": [{"name": "a"}, {"name": "b"}], "nodeName": "n1",
                     "initContainers": [{"name": "s", "restartPolicy": "Always"}]},
            "status": {"phase": "Running", "containerStatuses": [{"name": "a", "ready": true}],
                       "initContainerStatuses": [{"name": "s", "ready": true}]}
        });
        assert_eq!(
            cells(KubeKind::Pods, &pod),
            ["web", "default", "Running", "2/3", "n1"]
        );
        let svc = json!({
            "metadata": {"name": "api", "namespace": "default"},
            "spec": {"type": "NodePort", "clusterIPs": ["10.0.0.5"],
                     "ports": [{"port": 80, "nodePort": 30080, "protocol": "TCP"}, {"port": 53, "protocol": "UDP"}]}
        });
        assert_eq!(
            cells(KubeKind::Services, &svc),
            [
                "api",
                "default",
                "NodePort",
                "10.0.0.5",
                "80:30080/TCP, 53/UDP"
            ]
        );
        let route = json!({
            "metadata": {"name": "shop", "namespace": "default"},
            "spec": {"host": "shop.apps.ocp.example.com", "path": "/cart",
                     "to": {"kind": "Service", "name": "frontend"}, "tls": {"termination": "edge"}}
        });
        assert_eq!(
            cells(KubeKind::Routes, &route),
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
            route_link(&route).as_deref(),
            Some("https://shop.apps.ocp.example.com/cart")
        );
        let plain = json!({"metadata": {"name": "api", "namespace": "default"},
            "spec": {"host": "api.apps.ocp.example.com", "to": {"name": "api"}}});
        assert_eq!(cells(KubeKind::Routes, &plain)[3..], ["", "api", ""]);
        assert_eq!(
            route_link(&plain).as_deref(),
            Some("http://api.apps.ocp.example.com")
        );
        assert_eq!(route_link(&json!({"spec": {}})), None);
    }
}
