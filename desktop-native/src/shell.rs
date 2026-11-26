//! The window around the pages: the top bar, the sidebar (with its counts, the
//! Kubernetes sub-navigation, the VM card and the divider), the docker command banner
//! and the status bar. Then the main area, the panels, the dialogs and the toasts.

use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::layout::Row;
use podbag_ui::widgets::scroll::scroll_view;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{Color, CursorIcon, Id, Rect, icons};

use crate::app::{Desktop, KubeKind, Page, RAIL, SIDEBAR_WIDTH, SidebarDrag, clamp_width};
use crate::dialogs;
use crate::fmt;
use crate::icons::Icon;
use crate::lists::kind_index;
use crate::views::{self, settings, vm_state_key};
use podbag_desktop_core::Kind;

pub const SIDEBAR_TOGGLE: Id = Id(0x7368_656c_6c01);
pub const NAV_BACK: Id = Id(0x7368_656c_6c02);
pub const NAV_FORWARD: Id = Id(0x7368_656c_6c03);
pub const TOP_SETTINGS: Id = Id(0x7368_656c_6c04);
pub const SPLITTER: Id = Id(0x7368_656c_6c05);
pub const KUBE_TOGGLE: Id = Id(0x7368_656c_6c06);
pub const VM_BUTTON: Id = Id(0x7368_656c_6c07);

/// The id of a page's sidebar item.
pub fn nav_id(page: Page) -> Id {
    Id::of(("nav", page.name()))
}

pub fn kube_nav_id(kind: KubeKind) -> Id {
    Id::of(("nav-kube", kind.key()))
}

pub fn paint(d: &mut Desktop, ui: &mut Ui) {
    let t = ui.theme;
    let b = ui.bounds();
    ui.p.fill(b, t.page);
    let mut r = b;
    let top = r.cut_top(40.);
    let status = r.cut_bottom(27.);
    let banner = banner_shown(d).then(|| r.cut_bottom(36.));
    let side = r.cut_left(d.sidebar_width());
    // In the order of app.js's page, which is the Tab order.
    topbar(d, ui, top);
    sidebar(d, ui, side);
    splitter(d, ui, side);
    views::paint_main(d, ui, r);
    if let Some(banner) = banner {
        paint_banner(d, ui, banner);
    }
    statusbar(d, ui, status);
    if d.pull.is_some() {
        views::pull::paint(d, ui);
    }
    if d.dialog.is_some() {
        dialogs::paint(d, ui);
    }
    views::notices::paint(d, ui);
    d.toasts.paint(ui);
}

fn topbar(d: &mut Desktop, ui: &mut Ui, top: Rect) {
    let t = ui.theme;
    ui.p.border_bottom(top, t.line);
    let mut row = Row::new(top.inset(10., 0., 10., 1.), 2.);
    let mac = cfg!(target_os = "macos");
    let collapsed = d.collapsed();
    let label = d.t(if collapsed {
        "sidebar.show"
    } else {
        "sidebar.hide"
    });
    let tip = format!("{label} ({})", if mac { "⌃⌘S" } else { "Ctrl+B" });
    if button::icon_button(
        ui,
        SIDEBAR_TOGGLE,
        row.left_h(30., 30.),
        Icon::Sidebar.id(),
        &tip,
        IconKind::Plain,
        true,
    ) {
        d.toggle_sidebar();
    }
    let keys = if mac {
        ["⌘[", "⌘]"]
    } else {
        ["Alt+←", "Alt+→"]
    };
    for (id, icon, key, shortcut, step) in [
        (NAV_BACK, Icon::Back, "action.back", keys[0], -1),
        (NAV_FORWARD, Icon::Forward, "action.forward", keys[1], 1),
    ] {
        let tip = format!("{} ({shortcut})", d.t(key));
        let on = d.can_travel(step);
        if button::icon_button(
            ui,
            id,
            row.left_h(30., 30.),
            icon.id(),
            &tip,
            IconKind::Plain,
            on,
        ) {
            d.travel(step);
        }
    }
    let settings = row.right_h(30., 30.);
    if d.page == Page::Settings {
        ui.p.rounded(settings, 7., t.accent_soft);
    }
    let label = d.t("nav.settings");
    if button::icon_button(
        ui,
        TOP_SETTINGS,
        settings,
        Icon::Settings.id(),
        &label,
        IconKind::Plain,
        true,
    ) {
        d.go(Page::Settings);
    }
}

/// A sidebar item: a button with an icon, a label and a count. In the rail only the
/// icon shows, with the label as its tooltip.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nav_item(
    ui: &mut Ui,
    r: Rect,
    id: Id,
    icon: Icon,
    label: &str,
    count: Option<usize>,
    current: bool,
    collapsed: bool,
) -> bool {
    let t = ui.theme;
    let hovered = ui.hovered(id);
    if current {
        ui.p.rounded(r, 7., t.accent_soft);
    } else if hovered {
        ui.p.rounded(r, 7., t.hover);
    }
    let fg = if current {
        t.accent_text
    } else if hovered {
        t.ink
    } else {
        t.ink2
    };
    let icon_r = if collapsed {
        r.center(18., 18.)
    } else {
        Rect::new(r.x + 10., r.y + (r.h - 18.) / 2., 18., 18.)
    };
    ui.p.icon(icon.id(), icon_r, fg);
    if !collapsed {
        let mut inner = r.inset(38., 0., 10., 0.);
        if let Some(n) = count {
            let style = ui.style().with_size(12.);
            let text = ui.p.layout(&n.to_string(), style, None);
            let cr = inner.cut_right(text.width);
            let color = if current { t.accent_text } else { t.muted };
            ui.p.draw_text(&text, cr.x, cr.y + (cr.h - text.height) / 2., color);
            inner.w -= 6.;
        }
        let style = ui.style().weight(if current { 600 } else { 400 });
        ui.label(inner, label, style, fg);
    } else {
        let tip = match count {
            Some(n) => format!("{label} ({n})"),
            None => label.to_string(),
        };
        ui.tooltip(id, r, &tip);
    }
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r, 7.);
    ui.clicked(id)
}

fn sidebar(d: &mut Desktop, ui: &mut Ui, side: Rect) {
    let t = ui.theme;
    ui.p.fill(Rect::new(side.right() - 1., side.y, 1., side.h), t.line);
    // The items scroll when they do not fit (style.css `.sidebar`); Settings and the VM
    // card stay at the bottom while they do.
    let mut scroll = std::mem::take(&mut d.sidebar_scroll);
    let area = side.inset(0., 0., 1., 0.);
    scroll_view(
        ui,
        Id::of("sidebar-scroll"),
        area,
        &mut scroll,
        |ui, area| sidebar_items(d, ui, area),
    );
    d.sidebar_scroll = scroll;
}

/// The sidebar's items in `side` (scrolled); returns their height.
fn sidebar_items(d: &mut Desktop, ui: &mut Ui, side: Rect) -> f32 {
    let t = ui.theme;
    let collapsed = d.collapsed();
    let pad = if collapsed { 4. } else { 8. };
    let inner = side.inset(pad, 12., pad, 12.);
    let item_h = 34.;
    let mut y = inner.y;
    let item = |y: &mut f32, h: f32| {
        let r = Rect::new(inner.x, *y, inner.w, h);
        *y += h + 2.;
        r
    };
    // Migration (with its sources and kinds), the four lists, Training.
    views::migrate::nav(d, ui, inner, &mut y, collapsed);
    y += 4.;
    for (kind, icon) in [
        (Kind::Containers, Icon::Containers),
        (Kind::Images, Icon::Images),
        (Kind::Volumes, Icon::Volumes),
        (Kind::Networks, Icon::Networks),
    ] {
        let r = item(&mut y, item_h);
        let count =
            (d.loaded && d.lists.errors[kind_index(kind)].is_none()).then(|| d.lists.len(kind));
        let label = d.t(&format!("nav.{}", kind.segment()));
        let page = Page::of(kind);
        if nav_item(
            ui,
            r,
            nav_id(page),
            icon,
            &label,
            count,
            d.page == page,
            collapsed,
        ) {
            d.go(page);
        }
    }
    y += 4.;
    views::training::nav(d, ui, inner, &mut y, collapsed);
    y += 4.;
    // Kubernetes, with its resources below when expanded.
    let r = item(&mut y, item_h);
    let counts: Vec<Option<usize>> = KubeKind::ALL
        .iter()
        .map(|k| d.kube_counts.get(k.key()).copied())
        .collect();
    let total = counts
        .iter()
        .all(Option::is_some)
        .then(|| counts.iter().flatten().sum());
    let label = d.t("nav.kubernetes");
    let open_r = if collapsed {
        r
    } else {
        r.inset(0., 0., 32., 0.)
    };
    let active = d.page == Page::Kubernetes;
    if nav_item(
        ui,
        open_r,
        nav_id(Page::Kubernetes),
        Icon::Kubernetes,
        &label,
        total,
        active && !d.kube_expanded,
        collapsed,
    ) {
        d.go(Page::Kubernetes);
    }
    if !collapsed {
        let tr = Rect::new(r.right() - 30., r.y + 2., 30., 30.);
        if ui.hovered(KUBE_TOGGLE) {
            ui.p.bordered(tr, 7., t.hover, 1., t.line);
        }
        let icon = if d.kube_expanded {
            icons::CHEVRON_DOWN
        } else {
            icons::CHEVRON_RIGHT
        };
        ui.p.icon(icon, tr.center(14., 14.), t.ink2);
        ui.p.hitbox(tr, KUBE_TOGGLE, CursorIcon::Pointer);
        ui.focusable(KUBE_TOGGLE, FocusKind::Activate, tr, 7.);
        ui.tooltip(KUBE_TOGGLE, tr, &d.t("kube.resources"));
        if ui.clicked(KUBE_TOGGLE) {
            d.kube_expanded = !d.kube_expanded;
            if d.kube_expanded {
                d.refresh_kube_counts();
            }
        }
    }
    if d.kube_expanded {
        let sub_x = if collapsed { 0. } else { 28. };
        let top = y;
        for (n, (kind, icon)) in [
            (KubeKind::Pods, Icon::KubePods),
            (KubeKind::Services, Icon::KubeServices),
            (KubeKind::Deployments, Icon::KubeDeployments),
            (KubeKind::Claims, Icon::KubeClaims),
        ]
        .into_iter()
        .enumerate()
        {
            let r = item(&mut y, 30.).inset(sub_x, 0., 0., 0.);
            let label = d.t(&format!("kube.{}", kind.key()));
            let current = active && d.kube.kind == kind;
            if nav_item(
                ui,
                r,
                kube_nav_id(kind),
                icon,
                &label,
                counts[n],
                current,
                collapsed,
            ) {
                d.go_kube(kind);
            }
        }
        if !collapsed {
            ui.p.fill(Rect::new(inner.x + 18., top, 1., y - top - 2.), t.line);
        }
    }
    // The bottom: Settings, then the VM card or the engine line.
    let vm_card = d.vm.is_some() && d.has_vm();
    let below = if vm_card {
        vm_card_height(d, ui, collapsed)
    } else {
        32.
    };
    let bottom = (inner.bottom() - below).max(y + 8. + item_h + 8.);
    let r = Rect::new(inner.x, bottom - 8. - item_h, inner.w, item_h);
    let label = d.t("nav.settings");
    if nav_item(
        ui,
        r,
        nav_id(Page::Settings),
        Icon::Settings,
        &label,
        None,
        d.page == Page::Settings,
        collapsed,
    ) {
        d.go(Page::Settings);
    }
    let rest = Rect::new(inner.x, bottom, inner.w, below);
    if vm_card {
        paint_vm_card(d, ui, rest, collapsed);
    } else {
        engine_line(d, ui, rest, collapsed);
    }
    bottom + below + 12. - side.y
}

/// app.js `engineState`: the class and the text.
pub fn engine_state(d: &Desktop) -> (&'static str, String) {
    let Some(e) = &d.engine else {
        return ("checking", d.t("engine.checking"));
    };
    if e.engine != "podbag" {
        return if e.reachable {
            ("ok", d.t("engine.running"))
        } else {
            ("down", d.t("engine.down"))
        };
    }
    match (e.vm.as_deref(), e.reachable) {
        (Some("running"), _) => ("ok", d.t("engine.vmRunning")),
        (Some(_), false) => ("off", d.t("engine.vmStopped")),
        (_, true) => ("ok", d.t("engine.running")),
        _ => ("down", d.t("engine.down")),
    }
}

pub fn state_color(ui: &Ui, cls: &str) -> Color {
    let t = ui.theme;
    match cls {
        "ok" => t.green,
        "down" => t.red,
        "checking" => t.amber,
        _ => t.muted,
    }
}

fn engine_name(d: &Desktop) -> String {
    d.engine
        .as_ref()
        .map(|e| e.engine_name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "podbag".into())
}

fn engine_line(d: &mut Desktop, ui: &mut Ui, r: Rect, collapsed: bool) {
    let t = ui.theme;
    let (cls, text) = engine_state(d);
    let name = engine_name(d);
    ui.p.fill(Rect::new(r.x, r.y, r.w, 1.), t.line);
    let dot_x = if collapsed {
        r.x + (r.w - 8.) / 2.
    } else {
        r.x + 10.
    };
    let dot = Rect::new(dot_x, r.y + 16., 8., 8.);
    ui.p.rounded(dot, 4., state_color(ui, cls));
    let mut tip = format!("{name}: {text}");
    if let Some(more) = d
        .engine
        .as_ref()
        .and_then(|e| e.version.clone().or(e.error.clone()))
    {
        tip = format!("{tip}\n{more}");
    }
    if !collapsed {
        let style = ui.style().with_size(13.);
        let line = format!("{name} · {text}");
        ui.p.label(
            Rect::new(dot.right() + 8., r.y + 10., r.w - 28., 20.),
            &line,
            style,
            t.ink2,
        );
    }
    ui.tooltip(Id::of("engine-line"), r, &tip);
}

/// The rows of the VM card: (label, value).
fn vm_rows(d: &Desktop) -> Vec<(String, String)> {
    let Some(vm) = &d.vm else { return Vec::new() };
    let lang = d.lang();
    let running = vm.state == "running";
    let mut rows = Vec::new();
    if vm.containers.is_some() || running {
        rows.push((d.t("vm.containers"), vm.containers.unwrap_or(0).to_string()));
    }
    if vm.cpu_percent.is_some() || vm.in_use.is_some() {
        let mut parts = Vec::new();
        if let Some(p) = vm.cpu_percent {
            parts.push(fmt::percent(p, lang));
        }
        if let Some(c) = vm.in_use.and_then(|u| u.cpus) {
            parts.push(c.to_string());
        }
        rows.push((d.t("vm.cpu"), parts.join(" · ")));
    }
    if let Some(total) = vm.memory_total_mib.filter(|m| *m > 0) {
        let used = fmt::mib(Some(vm.memory_used_mib.unwrap_or(0)), lang);
        rows.push((
            d.t("vm.memory"),
            format!("{used} / {}", fmt::mib(Some(total), lang)),
        ));
    }
    rows
}

fn vm_card_height(d: &Desktop, _ui: &mut Ui, collapsed: bool) -> f32 {
    if collapsed {
        return 38.;
    }
    let meter =
        d.vm.as_ref()
            .is_some_and(|v| v.memory_total_mib.is_some_and(|m| m > 0));
    10. + 20. + 8. + vm_rows(d).len() as f32 * 21. + if meter { 12. } else { 0. } + 8. + 30. + 12.
}

fn paint_vm_card(d: &mut Desktop, ui: &mut Ui, r: Rect, collapsed: bool) {
    let t = ui.theme;
    let Some(vm) = d.vm.clone() else { return };
    let running = vm.state == "running";
    let stoppable = running || vm.state == "standby" || vm.state == "starting";
    let state_text = d.t(&format!("vm.state.{}", vm_state_key(&vm.state)));
    let title = d.t("vm.title");
    if collapsed {
        // The rail: the state as a dot, its text in the tooltip.
        let b = Rect::new(r.x + (r.w - 30.) / 2., r.bottom() - 34., 30., 30.);
        let tip = format!("{title}: {state_text}");
        if button::icon_button(
            ui,
            Id::of("vm-rail"),
            b,
            Icon::Vm.id(),
            &tip,
            IconKind::Plain,
            true,
        ) {
            d.open_section("vm");
        }
        let dot = Rect::new(b.right() - 10., b.bottom() - 10., 7., 7.);
        ui.p.rounded(dot.expand(2.), 5.5, t.page);
        ui.p.rounded(dot, 3.5, if running { t.green } else { t.muted });
        return;
    }
    let card = r.inset(0., 4., 0., 0.);
    ui.p.bordered(card, 10., t.surface, 1., t.line);
    let c = card.inset(12., 10., 12., 12.);
    let mut y = c.y;
    let small = ui.style().with_size(12.5).line_height(18.);
    ui.p.rounded(
        Rect::new(c.x, y + 6., 8., 8.),
        4.,
        if running { t.green } else { t.muted },
    );
    let head = ui.style().with_size(13.).weight(600).line_height(20.);
    let st = ui.p.layout(&state_text, small, Some(c.w * 0.55));
    ui.p.draw_text(&st, c.right() - st.width, y + 1., t.ink2);
    ui.label(
        Rect::new(c.x + 15., y, c.w - 20. - st.width, 20.),
        &title,
        head,
        t.ink,
    );
    ui.tooltip(
        Id::of("vm-head"),
        Rect::new(c.x, y, c.w, 20.),
        &format!("{title}: {state_text}"),
    );
    y += 28.;
    for (k, v) in vm_rows(d) {
        let vl = ui.p.layout(&v, small, Some(c.w * 0.65));
        ui.p.draw_text(&vl, c.right() - vl.width, y, t.ink);
        ui.p.label(
            Rect::new(c.x, y, c.w - vl.width - 6., 18.),
            &k,
            small,
            t.ink2,
        );
        y += 21.;
    }
    if let Some(total) = vm.memory_total_mib.filter(|m| *m > 0) {
        let used = (vm.memory_used_mib.unwrap_or(0) as f32 / total as f32).min(1.);
        let bar = Rect::new(c.x, y + 2., c.w, 4.);
        ui.p.rounded(bar, 2., t.sunken);
        ui.p.rounded(bar.with_w(bar.w * used), 2., t.accent);
        y += 12.;
    }
    y += 8.;
    let (label, kind, action) = if stoppable {
        (d.t("action.stopVm"), ButtonKind::Normal, "stop")
    } else {
        (d.t("action.startVm"), ButtonKind::Primary, "start")
    };
    let enabled = !d.vm_busy && vm.state != "not set up";
    if button::button(
        ui,
        VM_BUTTON,
        Rect::new(c.x, y, c.w, 30.),
        &label,
        None,
        kind,
        enabled,
    ) {
        d.vm_action(action);
    }
}

fn splitter(d: &mut Desktop, ui: &mut Ui, side: Rect) {
    let t = ui.theme;
    let r = Rect::new(side.right() - 5., side.y, 9., side.h);
    let focused = ui.has_focus(SPLITTER) && ui.focus_visible();
    if ui.hovered(SPLITTER) || d.sidebar_drag.is_some() || focused {
        ui.p.fill(Rect::new(r.x + 4., r.y, 1.5, r.h), t.accent);
    }
    ui.p.hitbox(r, SPLITTER, CursorIcon::ColResize);
    ui.focusable(
        SPLITTER,
        FocusKind::Group,
        Rect::new(r.x + 3., r.y, 3., r.h),
        1.,
    );
    ui.tooltip(SPLITTER, r, &d.t("sidebar.resize"));
    if let Some(press) = ui.pressed(SPLITTER) {
        if press.clicks == 2 {
            d.set_sidebar_width(SIDEBAR_WIDTH);
            return;
        }
        let width = d.sidebar_width();
        d.sidebar_drag = Some(SidebarDrag {
            x: press.pos.x,
            start: width,
            width,
        });
    }
    if let Some(drag) = &mut d.sidebar_drag {
        if ui.is_active(SPLITTER) {
            if let Some(p) = ui.pointer() {
                let w = drag.start + p.x - drag.x;
                let threshold = if drag.start == RAIL { 80. } else { 128. };
                drag.width = if w <= threshold { RAIL } else { clamp_width(w) };
                ui.repaint();
            }
        } else {
            let (start, width) = (drag.start, drag.width);
            d.sidebar_drag = None;
            if width != start {
                d.set_sidebar_width(width);
            }
        }
    }
    // The keyboard: arrows resize, Enter collapses, Home and End go to the ends.
    if d.sidebar_drag.is_none() {
        let width = d.sidebar_width();
        let step = 10.;
        let smaller = if width <= 160. {
            RAIL
        } else {
            (width - step).max(160.)
        };
        let larger = (width + step).max(160.);
        if ui.key_for(SPLITTER, NamedKey::Enter) {
            d.toggle_sidebar();
        } else if ui.key_for(SPLITTER, NamedKey::ArrowLeft)
            || ui.key_for(SPLITTER, NamedKey::ArrowDown)
        {
            d.set_sidebar_width(smaller);
        } else if ui.key_for(SPLITTER, NamedKey::ArrowRight)
            || ui.key_for(SPLITTER, NamedKey::ArrowUp)
        {
            d.set_sidebar_width(larger);
        } else if ui.key_for(SPLITTER, NamedKey::Home) {
            d.set_sidebar_width(RAIL);
        } else if ui.key_for(SPLITTER, NamedKey::End) {
            d.set_sidebar_width(crate::app::SIDEBAR_MAX);
        }
    }
}

fn banner_shown(d: &Desktop) -> bool {
    !d.banner_closed
        && d.docker
            .as_ref()
            .is_some_and(|c| c.installed && !c.is_podbag)
        && d.is_podbag()
        && d.reachable()
}

fn paint_banner(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let t = ui.theme;
    let Some(cmd) = d.docker.clone() else { return };
    ui.p.fill(r, t.amber_soft);
    ui.p.fill(r.with_h(1.), t.line);
    let message = if d.docker_switching.is_some() {
        d.docker_route()
    } else {
        d.tv("docker.banner", &[("name", &cmd.name)])
    };
    let warning = if cmd.from_env {
        format!(" {}", d.t("docker.env"))
    } else {
        String::new()
    };
    ui.p.icon(
        Icon::Warning.id(),
        Rect::new(r.x + 16., r.y + 9., 18., 18.),
        t.amber,
    );
    let close = Rect::new(r.right() - 40., r.y + 3., 30., 30.);
    let switch = Rect::new(close.x - 34., r.y + 3., 30., 30.);
    let text_r = Rect::new(r.x + 44., r.y, switch.x - r.x - 52., r.h);
    let style = ui.style().with_size(13.).weight(600);
    ui.label(text_r, &format!("{message}{warning}"), style, t.ink);
    if let Some((icon, to, label)) = settings::switch_button(d, &cmd)
        && button::icon_button(
            ui,
            Id::of("banner-switch"),
            switch,
            icon.id(),
            &label,
            IconKind::Accent,
            d.docker_switching.is_none(),
        )
    {
        d.switch_docker(to);
    }
    let label = d.t("pull.close");
    if button::icon_button(
        ui,
        Id::of("banner-close"),
        close,
        Icon::Close.id(),
        &label,
        IconKind::Plain,
        true,
    ) {
        d.banner_closed = true;
    }
}

/// One item of the status bar: text, an optional icon and dot, and an action.
struct Item {
    text: String,
    icon: Option<Icon>,
    dot: Option<Color>,
    id: Option<Id>,
    tip: String,
    accent: bool,
}

impl Item {
    fn new(text: String) -> Item {
        Item {
            tip: text.clone(),
            text,
            icon: None,
            dot: None,
            id: None,
            accent: false,
        }
    }

    fn width(&self, ui: &mut Ui) -> f32 {
        let style = ui.style().with_size(12.);
        let w = ui.p.layout(&self.text, style, None).width;
        w + 14. + self.icon.map_or(0., |_| 18.) + self.dot.map_or(0., |_| 13.)
    }

    /// Paints in `s`; true when clicked.
    fn paint(&self, ui: &mut Ui, s: Rect) -> bool {
        let t = ui.theme;
        let style = ui.style().with_size(12.);
        let hovered = self.id.is_some_and(|id| ui.hovered(id));
        let fg = if self.accent {
            ui.p.rounded(s, 5., t.accent_soft);
            t.accent_text
        } else if hovered {
            ui.p.rounded(s, 5., t.hover);
            t.ink
        } else {
            t.ink2
        };
        let mut x = s.x + 7.;
        if let Some(c) = self.dot {
            ui.p.rounded(Rect::new(x, s.y + (s.h - 8.) / 2., 8., 8.), 4., c);
            x += 13.;
        }
        if let Some(icon) = self.icon {
            ui.p.icon(
                icon.id(),
                Rect::new(x, s.y + (s.h - 13.) / 2., 13., 13.),
                fg,
            );
            x += 18.;
        }
        let layout = ui.p.layout(&self.text, style, None);
        ui.p.draw_text(&layout, x, s.y + (s.h - layout.height) / 2., fg);
        let tip_id = self.id.unwrap_or_else(|| Id::of(("status", &self.text)));
        ui.tooltip(tip_id, s, &self.tip);
        match self.id {
            Some(id) => {
                ui.p.hitbox(s, id, CursorIcon::Pointer);
                ui.focusable(id, FocusKind::Activate, s, 5.);
                ui.clicked(id)
            }
            None => false,
        }
    }
}

fn statusbar(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let t = ui.theme;
    let lang = d.lang();
    ui.p.fill(r.with_h(1.), t.line);
    let mut row = Row::new(r.inset(8., 3., 8., 2.), 2.);
    // The left side: the version, an update, a terminal.
    let mut left = Vec::new();
    if let Some(u) = &d.updates
        && let Some(engine) = &u.engine
    {
        let mut item = Item::new(format!("podbag {engine}"));
        item.tip = d.t("settings.updates");
        item.id = Some(Id::of("status-version"));
        left.push(("version", item));
        if u.newer
            && let Some(latest) = &u.latest
        {
            let mut item = Item::new(d.tv("status.available", &[("version", latest)]));
            item.icon = Some(Icon::Update);
            item.id = Some(Id::of("status-update"));
            item.accent = true;
            left.push(("version", item));
        }
    }
    let mut item = Item::new(d.t("status.terminal"));
    item.icon = Some(Icon::Shell);
    item.id = Some(Id::of("status-terminal"));
    left.push(("terminal", item));
    // A narrow window: the actions show their icons only, and the right side its
    // short values (style.css at 860 and 760 px); the tooltips keep the full texts.
    let window_w = ui.bounds().w;
    let (icons_only, compact) = (window_w <= 860., window_w <= 760.);
    if icons_only {
        for (_, item) in &mut left {
            if item.icon.is_some() {
                item.text.clear();
            }
        }
    }
    for (what, item) in left {
        let w = item.width(ui);
        if item.paint(ui, row.left(w)) {
            if what == "terminal" {
                d.open_terminal();
            } else {
                d.open_section("updates");
            }
        }
    }
    // The right side, in its order from the left.
    let mut right = Vec::new();
    let vm = d.vm.clone().filter(|_| d.has_vm());
    let (cls, text) = match &vm {
        Some(vm) => {
            let state = d.t(&format!("vm.state.{}", vm_state_key(&vm.state)));
            let cls = if vm.state == "running" { "ok" } else { "off" };
            (cls, format!("{}: {state}", d.t("vm.title")))
        }
        None => {
            let (cls, text) = engine_state(d);
            (cls, format!("{}: {text}", engine_name(d)))
        }
    };
    let mut item = Item::new(text);
    item.dot = Some(state_color(ui, cls));
    if compact {
        if vm.is_some() {
            item.icon = Some(Icon::Vm);
            item.text.clear();
        } else {
            item.text = engine_name(d);
        }
    }
    right.push(item);
    if d.loaded {
        let total = d.lists.containers.len().to_string();
        let running = d.lists.running().to_string();
        let mut item = Item::new(d.tv("status.running", &[("n", &running), ("total", &total)]));
        if compact {
            item.icon = Some(Icon::Containers);
            item.text = format!("{running}/{total}");
        }
        right.push(item);
    }
    if let Some(vm) = &d.vm
        && let Some(total) = vm.memory_total_mib.filter(|m| *m > 0)
    {
        let used = fmt::mib(Some(vm.memory_used_mib.unwrap_or(0)), lang);
        let mut item = Item::new(format!("{used} / {}", fmt::mib(Some(total), lang)));
        item.tip = format!("{} · {}", d.t("vm.memory"), item.text);
        if compact {
            item.text = used;
        }
        right.push(item);
    }
    // The docker command, with its switch (or the settings) button.
    let docker = d.docker.clone();
    let docker_item = docker.as_ref().map(|cmd| {
        if !cmd.installed {
            Item::new(d.t("docker.missingShort"))
        } else {
            let mut item = Item::new(format!("{} · {}", d.t("docker.command"), d.docker_route()));
            item.tip = if d.docker_switching.is_some() {
                d.docker_route()
            } else {
                d.tv("docker.status", &[("name", &cmd.name)])
            };
            item
        }
    });
    let button_w = if docker.as_ref().is_some_and(|c| c.installed) {
        26.
    } else {
        0.
    };
    let widths: Vec<f32> = right.iter().map(|i| i.width(ui)).collect();
    let docker_w = docker_item.as_ref().map_or(0., |i| i.width(ui) + button_w);
    let total: f32 = widths.iter().sum::<f32>() + docker_w + 2. * right.len() as f32;
    let rest = row.rest();
    let mut x = (rest.right() - total).max(rest.x);
    for (i, (item, w)) in right.iter().zip(&widths).enumerate() {
        let s = Rect::new(x, rest.y, *w, rest.h);
        if i > 0 {
            ui.p.fill(Rect::new(s.x - 2., s.y + (s.h - 12.) / 2., 1., 12.), t.line);
        }
        ui.clip(rest, |ui| item.paint(ui, s));
        x += w + 2.;
    }
    if let (Some(item), Some(cmd)) = (docker_item, docker) {
        let w = item.width(ui);
        let s = Rect::new(x, rest.y, w, rest.h);
        if !right.is_empty() {
            ui.p.fill(Rect::new(s.x - 2., s.y + (s.h - 12.) / 2., 1., 12.), t.line);
        }
        ui.clip(rest, |ui| item.paint(ui, s));
        if cmd.installed {
            let b = Rect::new(s.right(), rest.y + (rest.h - 20.) / 2., 24., 20.);
            match settings::switch_button(d, &cmd) {
                Some((icon, _, label)) => {
                    if button::icon_button(
                        ui,
                        Id::of("status-docker"),
                        b,
                        icon.id(),
                        &label,
                        IconKind::Accent,
                        d.docker_switching.is_none(),
                    ) {
                        settings::ask_switch(d, &cmd);
                    }
                }
                None => {
                    let label = d.t("nav.settings");
                    if button::icon_button(
                        ui,
                        Id::of("status-docker"),
                        b,
                        Icon::Settings.id(),
                        &label,
                        IconKind::Plain,
                        true,
                    ) {
                        d.open_section("engine");
                    }
                }
            }
        }
    }
}
