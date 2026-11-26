//! The four engine lists: the toolbar with the search box and the tools, the table
//! with each row's actions, and the empty states.

use podbag_desktop_core::Kind;
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::layout::Row;
use podbag_ui::widgets::table::{Column, Width, table};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{Id, Rect, TextStyle, VirtualList};

use crate::app::{Desktop, Tab};
use crate::fmt;
use crate::icons::Icon;
use crate::lists::{kind_index, ports_text, short_id};
use crate::views::{TOOLBAR_H, badge, centered, chip, code_line, title_bar};

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect, kind: Kind) {
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    toolbar(d, ui, bar, kind);
    let content = r.inset(20., 8., 20., 0.);
    list(d, ui, content, kind);
}

/// The id of a list's table (its rows are `table_id(kind).with(("row", i))`).
pub fn table_id(kind: Kind) -> Id {
    Id::of(("table", kind.segment()))
}

pub fn search_id(kind: Kind) -> Id {
    Id::of(("search", kind_index(kind)))
}

fn toolbar(d: &mut Desktop, ui: &mut Ui, bar: Rect, kind: Kind) {
    let t = ui.theme;
    let title = d.t(&format!("nav.{}", kind.segment()));
    let title_right = title_bar(ui, bar, &title);
    // The tools take their places from the right; they paint (and tab) left to right.
    let mut row = Row::new(bar.inset(20., 14., 20., 15.), 8.);
    let refresh = row.right_h(30., 30.);
    let prune_label = d.t("action.prune");
    let prune_w = button::width(ui, &prune_label, Some(Icon::Prune.id()), false);
    let prune = row.right_h(prune_w, 32.);
    let primary = match kind {
        Kind::Images => Some((d.t("action.pull"), Some(Icon::Pull.id()), "pull")),
        Kind::Volumes => Some((d.t("volume.create"), None, "create-volume")),
        _ => None,
    };
    let primary = primary.map(|(label, icon, key)| {
        let w = button::width(ui, &label, icon, false);
        (row.right_h(w, 32.), label, icon, key)
    });
    // Import from Docker. The web page shows Docker's logo here; `button::button` takes
    // glyphs only, so the import glyph stands in.
    let import = (kind == Kind::Images && crate::views::import::offered(d)).then(|| {
        let label = d.t("import.button");
        let w = button::width(ui, &label, Some(Icon::Import.id()), false);
        (row.right_h(w, 32.), label)
    });
    let live_text = if d.live {
        d.t("live.on")
    } else {
        String::new()
    };
    let style = ui.style().with_size(12.5);
    let lw = ui.p.layout(&live_text, style, None).width;
    let live = row.right_h(lw + 14., 20.);
    // The search box, after the title.
    let x = title_right + 12.;
    let w = (row.rest().right() - x - 12.).clamp(0., 280.);
    if w >= 120. {
        let search = Rect::new(x, bar.y + 15., w, 32.);
        let i = kind_index(kind);
        text_field(ui, &mut d.search[i], search, Some(Icon::Search.id()), true);
    }
    // The live dot: the engine's event stream is on.
    let color = if d.live { t.green } else { t.muted };
    ui.p.rounded(Rect::new(live.x, live.y + 6., 8., 8.), 4., color);
    ui.p.label(
        Rect::new(live.x + 14., live.y, lw + 2., 20.),
        &live_text,
        style,
        color,
    );
    let tip = d.t(if d.live { "live.on" } else { "live.off" });
    ui.tooltip(Id::of("live"), live, &tip);
    if let Some((rect, label)) = import
        && button::button(
            ui,
            crate::views::import::OPEN,
            rect,
            &label,
            Some(Icon::Import.id()),
            ButtonKind::Normal,
            true,
        )
    {
        crate::views::import::open(d);
    }
    if let Some((rect, label, icon, key)) = primary
        && button::button(
            ui,
            Id::of(key),
            rect,
            &label,
            icon,
            ButtonKind::Primary,
            true,
        )
    {
        match kind {
            Kind::Images => d.open_pull(),
            _ => d.ask_create_volume(),
        }
    }
    if button::button(
        ui,
        Id::of(("prune", kind.segment())),
        prune,
        &prune_label,
        Some(Icon::Prune.id()),
        ButtonKind::Normal,
        true,
    ) {
        d.ask_prune(kind);
    }
    let label = d.t("action.refresh");
    if button::icon_button(
        ui,
        Id::of("refresh"),
        refresh,
        Icon::Refresh.id(),
        &label,
        IconKind::Plain,
        true,
    ) {
        d.refresh();
    }
}

fn columns(kind: Kind, window_w: f32) -> Vec<(&'static str, Width)> {
    let narrow = window_w < 880.;
    let mid = window_w < 1060.;
    match kind {
        Kind::Containers => {
            let mut v = vec![
                ("col.name", Width::Flex(1.)),
                ("col.image", Width::Flex(1.)),
                ("col.state", Width::Px(128.)),
            ];
            if !narrow {
                v.push(("col.ports", Width::Px(130.)));
            }
            if !mid {
                v.push(("col.created", Width::Px(124.)));
            }
            v.push(("col.actions", Width::Px(164.)));
            v
        }
        Kind::Images => {
            let mut v = vec![("col.name", Width::Flex(1.))];
            if !narrow {
                v.push(("col.id", Width::Px(130.)));
            }
            v.extend([
                ("col.size", Width::Px(110.)),
                ("col.created", Width::Px(150.)),
                ("col.actions", Width::Px(56.)),
            ]);
            v
        }
        Kind::Volumes => vec![
            ("col.name", Width::Flex(1.)),
            ("col.driver", Width::Px(120.)),
            ("col.created", Width::Px(160.)),
            ("col.actions", Width::Px(56.)),
        ],
        Kind::Networks => {
            let mut v = vec![
                ("col.name", Width::Flex(1.)),
                ("col.driver", Width::Px(110.)),
                ("col.subnet", Width::Flex(1.)),
            ];
            if !narrow {
                v.push(("col.id", Width::Px(130.)));
            }
            v.push(("col.actions", Width::Px(56.)));
            v
        }
    }
}

fn list(d: &mut Desktop, ui: &mut Ui, r: Rect, kind: Kind) {
    let t = ui.theme;
    let i = kind_index(kind);
    if !d.loaded {
        let text = d.t("engine.checking");
        ui.p.label(
            Rect::new(r.x + 10., r.y + 24., r.w, 20.),
            &text,
            ui.style(),
            t.ink2,
        );
        return;
    }
    if let Some(err) = d.lists.errors[i].clone() {
        let h = ui.paragraph(r.x + 10., r.y + 24., r.w - 20., &err, ui.style(), t.red);
        let label = d.t("action.retry");
        let w = button::width(ui, &label, Some(Icon::Refresh.id()), false);
        if button::button(
            ui,
            Id::of("list-retry"),
            Rect::new(r.x + 10., r.y + 36. + h, w, 32.),
            &label,
            Some(Icon::Refresh.id()),
            ButtonKind::Normal,
            true,
        ) {
            d.refresh();
        }
        return;
    }
    if d.lists.len(kind) == 0 {
        empty_state(d, ui, r, kind);
        return;
    }
    let q = d.search[i].content().to_string();
    let shown = d.lists.filter(kind, &q).to_vec();
    if shown.is_empty() {
        let text = d.tv("empty.search", &[("q", q.trim())]);
        ui.paragraph(r.x + 10., r.y + 24., r.w - 20., &text, ui.style(), t.ink2);
        return;
    }
    let cols = columns(kind, ui.bounds().w);
    let labels: Vec<String> = cols
        .iter()
        .map(|(k, _)| {
            if *k == "col.actions" {
                String::new()
            } else {
                d.t(k)
            }
        })
        .collect();
    let table_cols: Vec<Column> = cols
        .iter()
        .zip(&labels)
        .map(|((_, w), l)| Column {
            label: l.as_str(),
            width: *w,
        })
        .collect();
    let keys: Vec<&'static str> = cols.iter().map(|(k, _)| *k).collect();
    let id = table_id(kind);
    let mut rows = std::mem::replace(&mut d.tables[i], VirtualList::new(1., 1.));
    let clicked = table(
        ui,
        id,
        r,
        &table_cols,
        shown.len(),
        &mut rows,
        |ui, row, col, cell| {
            let row_id = id.with(("row", row));
            paint_cell(d, ui, kind, shown[row], keys[col], cell, row_id);
        },
    );
    d.tables[i] = rows;
    if let Some(row) = clicked {
        let oid = d.lists.id(kind, shown[row]);
        d.open_detail(kind, &oid, Tab::Info);
    }
}

fn paint_cell(
    d: &mut Desktop,
    ui: &mut Ui,
    kind: Kind,
    i: usize,
    key: &str,
    cell: Rect,
    row_id: Id,
) {
    let t = ui.theme;
    let base = ui.style();
    let primary = base.weight(600);
    let mono = TextStyle::new(12.5).mono().line_height(18.);
    let small_mono = TextStyle::new(12.).mono().line_height(16.);
    if key == "col.actions" {
        row_actions(d, ui, kind, i, cell, row_id, false);
        return;
    }
    // The name is a link: Tab reaches it and Enter opens the object's page.
    let link = |ui: &mut Ui, r: Rect| ui.focusable(row_id, FocusKind::Activate, r, 4.);
    match (kind, key) {
        (Kind::Containers, "col.name") => {
            let c = &d.lists.containers[i];
            let name_r = Rect::new(cell.x, cell.y + 9., cell.w, 20.);
            let name = crate::lists::container_name(c);
            let id = short_id(&c.id).to_string();
            ui.label(name_r, &name, primary, t.ink);
            ui.p.label(
                Rect::new(cell.x, name_r.bottom() + 1., cell.w, 16.),
                &id,
                small_mono,
                t.ink2,
            );
            let w = ui.p.layout(&name, primary, None).width.min(cell.w);
            link(ui, name_r.with_w(w));
        }
        (Kind::Containers, "col.image") => {
            let image = d.lists.containers[i].image.clone();
            ui.label(cell, &image, base, t.ink);
        }
        (Kind::Containers, "col.state") => {
            let state = d.lists.containers[i].state.clone();
            ui.clip(cell, |ui| {
                chip(d, ui, cell.x, cell.y + cell.h / 2., &state, cell.w)
            });
        }
        (Kind::Containers, "col.ports") => {
            let ports = ports_text(&d.lists.containers[i]);
            let text = if ports.is_empty() { "—" } else { &ports };
            ui.label(cell, text, mono, t.ink);
        }
        (Kind::Containers, "col.created") => {
            let text = d.ago(d.lists.containers[i].created);
            ui.label(cell, &text, base, t.ink2);
        }
        (Kind::Images, "col.name") | (Kind::Volumes, "col.name") => {
            let name = d.lists.name(kind, i);
            let w = ui.p.layout(&name, primary, None).width.min(cell.w);
            ui.label(cell, &name, primary, t.ink);
            link(ui, cell.center_v(20.).with_w(w));
        }
        (Kind::Images, "col.id") => {
            let id = short_id(&d.lists.images[i].id).to_string();
            ui.p.label(cell, &id, mono, t.ink2);
        }
        (Kind::Images, "col.size") => {
            let text = fmt::size(d.lists.images[i].size.max(0) as u64, d.lang());
            ui.p.label(cell, &text, base, t.ink);
        }
        (Kind::Images, "col.created") => {
            let text = d.ago(d.lists.images[i].created);
            ui.p.label(cell, &text, base, t.ink2);
        }
        (Kind::Volumes, "col.driver") => {
            let text = d.lists.volumes[i].driver.clone();
            ui.label(cell, &text, base, t.ink);
        }
        (Kind::Volumes, "col.created") => {
            let when = fmt::parse_rfc3339(&d.lists.volumes[i].created_at);
            let text = d.ago(when);
            ui.p.label(cell, &text, base, t.ink2);
        }
        (Kind::Networks, "col.name") => {
            let n = &d.lists.networks[i];
            let builtin = n.builtin();
            let name = ui.p.layout(&n.name, primary, Some(cell.w));
            let ny = cell.y + (cell.h - name.height) / 2.;
            ui.p.draw_text(&name, cell.x, ny, t.ink);
            link(ui, Rect::new(cell.x, ny, name.width, name.height));
            if builtin {
                let tag = d.t("state.builtin");
                let style = TextStyle::new(11.).line_height(16.);
                let tl = ui.p.layout(&tag, style, None);
                let tr = Rect::new(
                    cell.x + name.width + 8.,
                    cell.y + (cell.h - 16.) / 2.,
                    tl.width + 12.,
                    16.,
                );
                if tr.right() < cell.right() {
                    ui.p.rounded(tr, 4., t.gray_soft);
                    ui.p.draw_text(&tl, tr.x + 6., tr.y, t.ink2);
                }
            }
        }
        (Kind::Networks, "col.driver") => {
            let text = d.lists.networks[i].driver.clone();
            ui.label(cell, &text, base, t.ink);
        }
        (Kind::Networks, "col.subnet") => {
            let s = d.lists.networks[i].subnet().to_string();
            ui.label(cell, if s.is_empty() { "—" } else { &s }, mono, t.ink);
        }
        (Kind::Networks, "col.id") => {
            let id = short_id(&d.lists.networks[i].id).to_string();
            ui.p.label(cell, &id, mono, t.ink2);
        }
        _ => {}
    }
}

/// app.js `rowActions`: icon buttons, right-aligned in `cell`. `detail`: the object's
/// page (no Logs; Rename where the engine has it).
pub fn row_actions(
    d: &mut Desktop,
    ui: &mut Ui,
    kind: Kind,
    i: usize,
    cell: Rect,
    base_id: Id,
    detail: bool,
) {
    let id = d.lists.id(kind, i);
    let name = d.lists.name(kind, i);
    let busy = d.busy.contains(&id);
    let mut buttons: Vec<(&'static str, Icon, IconKind, &'static str)> = Vec::new();
    if kind == Kind::Containers {
        let c = &d.lists.containers[i];
        match c.state.as_str() {
            "running" | "restarting" => {
                buttons.push(("stop", Icon::Stop, IconKind::Plain, "action.stop"));
                buttons.push(("restart", Icon::Restart, IconKind::Plain, "action.restart"));
                if d.features.pause {
                    buttons.push(("pause", Icon::Pause, IconKind::Plain, "action.pause"));
                }
            }
            "paused" => {
                if d.features.pause {
                    buttons.push(("unpause", Icon::Start, IconKind::Plain, "action.resume"));
                }
                buttons.push(("stop", Icon::Stop, IconKind::Plain, "action.stop"));
            }
            _ => buttons.push(("start", Icon::Start, IconKind::Accent, "action.start")),
        }
        if !detail {
            buttons.push(("logs", Icon::Logs, IconKind::Plain, "action.logs"));
        }
        if c.state == "running" {
            buttons.push(("shell", Icon::Shell, IconKind::Plain, "action.shell"));
        }
    }
    let builtin = kind == Kind::Networks && d.lists.networks[i].builtin();
    if !builtin {
        buttons.push(("remove", Icon::Remove, IconKind::Danger, "action.remove"));
    }
    if detail && kind == Kind::Containers && d.features.rename {
        buttons.push(("rename", Icon::Rename, IconKind::Plain, "action.rename"));
    }
    let n = buttons.len() as f32;
    let mut x = cell.right() - n * 32. + 2.;
    let y = cell.y + (cell.h - 30.) / 2.;
    let mut chosen = None;
    for (action, icon, ik, label) in buttons {
        let r = Rect::new(x, y, 30., 30.);
        x += 32.;
        let label = d.t(label);
        if button::icon_button(ui, base_id.with(action), r, icon.id(), &label, ik, !busy) {
            chosen = Some(action);
        }
    }
    let Some(action) = chosen else { return };
    match action {
        "logs" => d.open_detail(kind, &id, Tab::Logs),
        "shell" => d.open_shell(&id),
        "rename" => d.ask_rename(i),
        "remove" => d.ask_remove(kind, i),
        action => d.act(&id, &name, action),
    }
}

fn empty_state(d: &mut Desktop, ui: &mut Ui, r: Rect, kind: Kind) {
    let t = ui.theme;
    let cx = r.x + r.w / 2.;
    let max_w = r.w - 40.;
    let mut y = r.y + 48.;
    let icon = match kind {
        Kind::Containers => Icon::Containers,
        Kind::Images => Icon::Images,
        Kind::Volumes => Icon::Volumes,
        Kind::Networks => Icon::Networks,
    };
    badge(ui, cx, y, icon);
    y += 80.;
    let h2 = ui.style().with_size(18.).weight(700);
    let title = d.t(&format!("empty.{}.title", kind.segment()));
    y += centered(ui, cx, y, max_w, &title, h2, t.ink) + 8.;
    let podbag = d.is_podbag();
    let docker = d.engine.as_ref().is_some_and(|e| e.engine == "docker");
    if podbag || docker || kind == Kind::Images {
        let text = d.t(&format!("empty.{}.text", kind.segment()));
        y += centered(ui, cx, y, max_w, &text, ui.style(), t.ink2) + 8.;
    }
    let cmd = if podbag { "podbag" } else { "docker" };
    let command = match kind {
        Kind::Containers => Some(format!("{cmd} run -d --name web -p 8080:80 nginx:alpine")),
        Kind::Volumes => Some(format!("{cmd} volume create data")),
        Kind::Networks => Some(format!("{cmd} network create app")),
        Kind::Images => None,
    };
    if let Some(command) = command.filter(|_| podbag || docker) {
        y += code_line(d, ui, cx, y, &command, Id::of("empty-code")) + 14.;
    }
    if kind == Kind::Images {
        let label = d.t("action.pull");
        let w = button::width(ui, &label, Some(Icon::Pull.id()), false);
        if button::button(
            ui,
            Id::of("empty-pull"),
            Rect::new(cx - w / 2., y + 6., w, 32.),
            &label,
            Some(Icon::Pull.id()),
            ButtonKind::Primary,
            true,
        ) {
            d.open_pull();
        }
    }
}
