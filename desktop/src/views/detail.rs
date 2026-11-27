//! An object's page: the header with its actions, the tabs, and the info, logs or
//! inspect panel.

use podbag_desktop_core::Kind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::logs::{follow_now, log_view};
use podbag_ui::widgets::scroll::scroll_view;
use podbag_ui::widgets::selectable::selectable_text;
use podbag_ui::widgets::tabs::tabs;
use podbag_ui::{Id, Point, Rect, TextStyle};

use crate::app::{Desktop, Tab};
use crate::fmt;
use crate::icons::Icon;
use crate::lists::{container_name, gateway, ports_text};
use crate::views::resources::row_actions;
use crate::views::{Field, TOOLBAR_H, chip, field, fields};

pub const BACK: Id = Id(0x0064_6574_6169_6c01);
pub const TABS: Id = Id(0x0064_6574_6169_6c02);
pub const FOLLOW: Id = Id(0x0064_6574_6169_6c03);
pub const WRAP: Id = Id(0x0064_6574_6169_6c04);
pub const LOGS: Id = Id(0x0064_6574_6169_6c05);
pub const ACTIONS: Id = Id(0x0064_6574_6169_6c06);

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let t = ui.theme;
    let Some((kind, id, tab)) = d.detail.as_ref().map(|x| (x.kind, x.id.clone(), x.tab)) else {
        return;
    };
    let Some(i) = d.lists.find(kind, &id) else {
        // Not (yet) in the lists: the page waits for them.
        let text = d.t("engine.checking");
        ui.p.label(
            Rect::new(r.x + 30., r.y + 24., r.w, 20.),
            &text,
            ui.style(),
            t.ink2,
        );
        return;
    };
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    ui.p.border_bottom(bar, t.line);
    let inner = bar.inset(12., 0., 20., 1.);
    let back = d.t("action.back");
    let back_r = Rect::new(inner.x, inner.y + (inner.h - 30.) / 2., 30., 30.);
    if button::icon_button(
        ui,
        BACK,
        back_r,
        Icon::Back.id(),
        &back,
        IconKind::Plain,
        true,
    ) {
        d.close_detail();
        return;
    }
    // The title takes what the actions leave.
    let name = d.lists.name(kind, i);
    let style = ui.style().with_size(18.).weight(700);
    let actions_w = 6. * 32.;
    let max_title = (inner.w - actions_w - 42. - 150.).max(60.);
    let title = ui.p.layout(&name, style, Some(max_title));
    let tx = inner.x + 42.;
    ui.p.draw_text(&title, tx, inner.y + (inner.h - title.height) / 2., t.ink);
    ui.tooltip(
        Id::of(("detail-title", &name)),
        Rect::new(tx, inner.y, title.width, inner.h),
        &name,
    );
    if kind == Kind::Containers {
        let state = d.lists.containers[i].state.clone();
        chip(
            d,
            ui,
            tx + title.width + 12.,
            inner.y + inner.h / 2.,
            &state,
            140.,
        );
    }
    let actions = Rect::new(inner.right() - actions_w, inner.y, actions_w, inner.h);
    row_actions(d, ui, kind, i, actions, ACTIONS, true);
    if d.detail.as_ref().is_none_or(|x| x.id != id || x.tab != tab) {
        return; // an action left or changed the page
    }
    // The tabs.
    let list: Vec<Tab> = if kind == Kind::Containers {
        vec![Tab::Info, Tab::Logs, Tab::Inspect]
    } else {
        vec![Tab::Info, Tab::Inspect]
    };
    let labels: Vec<String> = list.iter().map(|t| d.t(t.key())).collect();
    let tab_bar = r.cut_top(42.);
    ui.p.border_bottom(tab_bar, t.line);
    let selected = list.iter().position(|x| *x == tab).unwrap_or(0);
    if let Some(n) = tabs(ui, TABS, tab_bar.inset(20., 0., 20., 0.), &labels, selected) {
        d.set_tab(list[n]);
        return;
    }
    let panel = r.inset(20., 18., 20., 20.);
    match tab {
        Tab::Info => info(d, ui, panel, kind, i),
        Tab::Logs => logs(d, ui, panel),
        Tab::Inspect => {
            let Some(det) = &mut d.detail else { return };
            log_view(ui, Id::of("inspect"), panel, &mut det.inspect, "");
        }
    }
}

fn info(d: &mut Desktop, ui: &mut Ui, panel: Rect, kind: Kind, i: usize) {
    let lang = d.lang();
    let row = |key: &str, v: String, mono: bool| field(d.t(key), v, mono);
    let rows: Vec<Field> = match kind {
        Kind::Containers => {
            let c = &d.lists.containers[i];
            vec![
                row("col.id", c.id.clone(), true),
                row("col.image", c.image.clone(), false),
                row("detail.command", c.command.clone(), true),
                row("detail.status", c.status.clone(), false),
                row("col.ports", ports_text(c), false),
                row("col.created", d.ago(c.created), false),
            ]
        }
        Kind::Images => {
            let o = &d.lists.images[i];
            let users: Vec<String> = d
                .lists
                .containers
                .iter()
                .filter(|c| o.repo_tags.contains(&c.image) || c.image == o.id)
                .map(container_name)
                .collect();
            let mut rows = vec![
                row("col.id", o.id.clone(), true),
                row("detail.tags", o.repo_tags.join(", "), false),
                row("col.size", fmt::size(o.size.max(0) as u64, lang), false),
            ];
            let sizes = crate::views::compression::of(o.podbag_compression.as_ref());
            if let Some((stored, original, saved)) = sizes {
                let (stored, before) = (fmt::size(stored, lang), fmt::size(original, lang));
                let text = d.tv(
                    "compression.stored",
                    &[("size", &stored), ("before", &before)],
                );
                let percent = fmt::percent(saved as f64, lang);
                let mut f = row("detail.compression", text, false);
                f.note = d.tv("compression.smaller", &[("percent", &percent)]);
                rows.push(f);
            }
            rows.push(row("col.created", d.ago(o.created), false));
            rows.push(row("detail.used", users.join(", "), false));
            rows
        }
        Kind::Volumes => {
            let o = &d.lists.volumes[i];
            let when = fmt::parse_rfc3339(&o.created_at);
            vec![
                row("col.name", o.name.clone(), false),
                row("col.driver", o.driver.clone(), false),
                row("detail.path", o.mountpoint.clone(), true),
                row("col.created", d.ago(when), false),
            ]
        }
        Kind::Networks => {
            let o = &d.lists.networks[i];
            vec![
                row("col.id", o.id.clone(), true),
                row("col.driver", o.driver.clone(), false),
                row("col.subnet", o.subnet().to_string(), true),
                row("detail.gateway", gateway(o).to_string(), true),
                row("detail.scope", o.scope.clone(), false),
            ]
        }
    };
    let none = d.t("detail.none");
    let running = kind == Kind::Containers && d.lists.containers[i].state == "running";
    let mut extras: Vec<(String, String)> = Vec::new();
    if running && let Some(det) = &d.detail {
        for (on, label, text) in [
            (d.features.top, "detail.processes", &det.top),
            (d.features.stats, "detail.usage", &det.stats),
        ] {
            if on {
                extras.push((d.t(label), text.clone().unwrap_or_default()));
            }
        }
    }
    let t = ui.theme;
    let mut scroll = std::mem::take(&mut d.detail_scroll);
    scroll_view(
        ui,
        Id::of("detail-info"),
        panel.inset(0., -18., 0., 0.),
        &mut scroll,
        |ui, area| {
            let mut y = area.y + 18.;
            y += fields(
                ui,
                Id::of("detail-fields"),
                area.x,
                y,
                area.w - 12.,
                &rows,
                &none,
            );
            for (n, (title, text)) in extras.iter().enumerate() {
                y += 12.;
                let style = ui.style().weight(700);
                ui.p.label(Rect::new(area.x, y, area.w, 20.), title, style, t.ink);
                y += 28.;
                let mono = TextStyle::new(12.5).mono().line_height(19.);
                let h = ui.p.layout_wrapped(text, mono, area.w - 40.).height + 24.;
                ui.p.bordered(
                    Rect::new(area.x, y, area.w - 12., h),
                    8.,
                    t.surface,
                    1.,
                    t.line,
                );
                selectable_text(
                    ui,
                    Id::of(("detail-json", n)),
                    Point::new(area.x + 14., y + 12.),
                    area.w - 40.,
                    text,
                    mono,
                    t.ink,
                );
                y += h;
            }
            y + 24. - area.y
        },
    );
    d.detail_scroll = scroll;
}

fn logs(d: &mut Desktop, ui: &mut Ui, panel: Rect) {
    let mut panel = panel;
    let bar = panel.cut_top(28.);
    panel.cut_top(8.);
    let follow_label = d.t("action.follow");
    let wrap_label = d.t("action.wrap");
    let fw = checkbox::width(ui, &follow_label);
    let ww = checkbox::width(ui, &wrap_label);
    let mut follow = d.logs.follow;
    let wrap_r = Rect::new(bar.right() - ww, bar.y, ww, bar.h);
    let follow_r = Rect::new(wrap_r.x - 16. - fw, bar.y, fw, bar.h);
    if checkbox::checkbox(ui, FOLLOW, follow_r, Some(&mut follow), &follow_label, true) {
        d.logs.follow = follow;
        if follow {
            follow_now(&mut d.logs);
        }
    }
    let mut wrap = d.logs.wrap;
    if checkbox::checkbox(ui, WRAP, wrap_r, Some(&mut wrap), &wrap_label, true) {
        d.logs.wrap = wrap;
        if d.logs.follow {
            follow_now(&mut d.logs);
        }
    }
    let empty = d.logs_error.clone().unwrap_or_else(|| d.t("detail.noLogs"));
    log_view(ui, LOGS, panel, &mut d.logs, &empty);
}
