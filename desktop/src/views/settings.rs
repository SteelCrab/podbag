//! The settings page: a search, then startup, language and theme; then the engine
//! (with the docker command), the VM (with the shared folders on a Mac), the
//! synchronized shares, updates and about, as sections that open and close.

use podbag_desktop_core::ThemePreference;
use podbag_desktop_core::vm::{VmSettings, VmStatus};
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::radio::segmented;
use podbag_ui::widgets::scroll::scroll_view;
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::slider::slider;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{CursorIcon, Id, Rect, icons, rgb};

use crate::app::Desktop;
use crate::dialogs::{Dialog, Then};
use crate::fmt;
use crate::icons::Icon;
use crate::theme::setting_color;
use crate::tr::language_names;
use crate::views::{TOOLBAR_H, field, fields, pill, title_bar, vm_state_key};

const ROW_H: f32 = 61.;

pub const LANGUAGE: Id = Id(0x0073_6574_7401);
pub const THEME: Id = Id(0x0073_6574_7402);
pub const LAUNCH: Id = Id(0x0073_6574_7403);
pub const ENGINE: Id = Id(0x0073_6574_7404);
pub const APPLY: Id = Id(0x0073_6574_7405);
pub const ADD_SHARE: Id = Id(0x0073_6574_7406);

/// The Remove button of a shared folder.
pub fn remove_share_id(path: &str) -> Id {
    Id::of(("vm-share-remove", path))
}

pub fn section_id(key: &str) -> Id {
    Id::of(("section", key))
}

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    let title = d.t("settings.title");
    title_bar(ui, bar, &title);
    let mut scroll = std::mem::take(&mut d.settings_scroll);
    scroll_view(ui, Id::of("settings-scroll"), r, &mut scroll, |ui, area| {
        let w = (area.w - 40.).min(780.);
        let x = area.x + 20.;
        let mut y = area.y + 20.;
        y += crate::views::settings_search::paint(d, ui, x, y, w) + 18.;
        // While the search has text, its results show instead of the settings.
        if d.settings_search.searching() {
            return y + 6. - area.y;
        }
        y += card(ui, 1, x, y, w, |ui, x, y, w| first_card(d, ui, x, y, w)) + 18.;
        y += card(ui, 2, x, y, w, |ui, x, y, w| second_card(d, ui, x, y, w)) + 24.;
        y - area.y
    });
    d.settings_scroll = scroll;
}

/// A card; `body` paints inside (16 px side padding) and returns its height.
fn card(
    ui: &mut Ui,
    n: usize,
    x: f32,
    y: f32,
    w: f32,
    body: impl FnOnce(&mut Ui, f32, f32, f32) -> f32,
) -> f32 {
    let t = ui.theme;
    let id = Id::of(("settings-card", n));
    let h = ui.mem.heights.get(&id).copied().unwrap_or(ROW_H * 3.);
    ui.p.bordered(Rect::new(x, y, w, h), 10., t.surface, 1., t.line);
    let used = body(ui, x + 16., y, w - 32.);
    if (used - h).abs() > 0.5 {
        ui.mem.heights.insert(id, used);
        ui.repaint();
    }
    used
}

/// The colored icon and title of a setting; returns its width.
fn setting_title(ui: &mut Ui, x: f32, cy: f32, key: &str, icon: Icon, label: &str) -> f32 {
    let t = ui.theme;
    let ir = Rect::new(x, cy - 16., 32., 32.);
    ui.p.rounded(ir, 8., setting_color(key));
    ui.p.icon(icon.id(), ir.center(20., 20.), rgb(0xffffff));
    let style = ui.style().weight(600);
    let l = ui.p.layout(label, style, None);
    ui.p.draw_text(&l, x + 44., cy - l.height / 2., t.ink);
    44. + l.width
}

fn separator(ui: &mut Ui, x: f32, y: f32, w: f32) {
    let line = ui.theme.line;
    ui.p.fill(Rect::new(x, y, w, 1.), line);
}

pub(super) fn form_field(ui: &mut Ui, x: f32, y: &mut f32, w: f32, label: &str) -> Rect {
    let t = ui.theme;
    *y += ui.paragraph(x, *y, w, label, ui.style(), t.ink) + 4.;
    let rect = Rect::new(x, *y, w.min(350.), 34.);
    *y += 43.;
    rect
}

fn first_card(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let mut y = y0;
    // Startup: launch at login. A failed read shows the error and Retry.
    let cy = y + ROW_H / 2.;
    setting_title(ui, x, cy, "startup", Icon::Power, &d.t("settings.startup"));
    let label = d.t("settings.launchAtLogin");
    let cw = checkbox::width(ui, &label).min(w * 0.6);
    let r = Rect::new(x + w - cw, y, cw, ROW_H);
    match d.launch_at_login() {
        Ok(mut on) => {
            if checkbox::checkbox(ui, LAUNCH, r, Some(&mut on), &label, true) {
                d.set_launch_at_login(on);
            }
        }
        Err(e) => {
            checkbox::checkbox(ui, LAUNCH, r, None, &label, false);
            ui.tooltip(LAUNCH, r, &e);
            let retry = d.t("action.retry");
            let bw = button::width(ui, &retry, None, false);
            let br = Rect::new(r.x - 8. - bw, cy - 14., bw, 28.);
            if button::button(
                ui,
                LAUNCH.with("retry"),
                br,
                &retry,
                None,
                ButtonKind::Normal,
                true,
            ) {
                d.reread_launch_at_login();
            }
        }
    }
    y += ROW_H;
    separator(ui, x, y, w);
    // Language.
    let cy = y + ROW_H / 2.;
    setting_title(
        ui,
        x,
        cy,
        "language",
        Icon::Globe,
        &d.t("settings.language"),
    );
    let names = language_names();
    let system = d.t("settings.system");
    let mut items = vec![Item::Option(&system)];
    items.extend(names.iter().map(|(_, n)| Item::Option(n.as_str())));
    let sel = names
        .iter()
        .position(|(l, _)| *l == d.settings.language)
        .map_or(0, |i| i + 1);
    let sw = 190f32.min(w * 0.5);
    if let Some(i) = select(
        ui,
        LANGUAGE,
        Rect::new(x + w - sw, cy - 17., sw, 34.),
        &items,
        sel,
        true,
    ) {
        let setting = if i == 0 { "system" } else { names[i - 1].0 };
        d.set_language(setting);
    }
    y += ROW_H;
    separator(ui, x, y, w);
    // Theme.
    let cy = y + ROW_H / 2.;
    setting_title(ui, x, cy, "theme", Icon::Appearance, &d.t("settings.theme"));
    let options = [
        (ThemePreference::System, "system", Icon::Display),
        (ThemePreference::Light, "light", Icon::Sun),
        (ThemePreference::Dark, "dark", Icon::Moon),
    ];
    let labels: Vec<String> = options
        .iter()
        .map(|(_, k, _)| d.t(&format!("settings.{k}")))
        .collect();
    let segs: Vec<(Option<u16>, &str)> = options
        .iter()
        .zip(&labels)
        .map(|((_, _, i), l)| (Some(i.id()), l.as_str()))
        .collect();
    let sel = options
        .iter()
        .position(|(p, _, _)| *p == d.settings.theme)
        .unwrap_or(0);
    let seg_style = ui.style().with_size(13.).weight(600);
    // Equal segments, each as wide as the longest label needs.
    let widest = labels
        .iter()
        .map(|l| ui.p.layout(l, seg_style, None).width)
        .fold(0f32, f32::max);
    let natural = (widest + 21. + 24.) * labels.len() as f32 + 4.;
    let sw = natural.max(240.).min(w * 0.7);
    if let Some(i) = segmented(
        ui,
        THEME,
        Rect::new(x + w - sw, cy - 16., sw, 32.),
        &segs,
        sel,
    ) {
        d.set_theme(options[i].0);
    }
    y + ROW_H - y0
}

/// A section header that opens and closes (a `details` summary); returns its height
/// and whether it is open.
#[allow(clippy::too_many_arguments)]
fn section(
    d: &mut Desktop,
    ui: &mut Ui,
    x: f32,
    y: f32,
    w: f32,
    key: &'static str,
    icon: Icon,
    value: &str,
) -> (f32, bool) {
    let t = ui.theme;
    let id = section_id(key);
    let r = Rect::new(x - 8., y, w + 16., ROW_H);
    if ui.hovered(id) {
        ui.p.rounded(r.inset(0., 2., 0., 2.), 6., t.hover);
    }
    let cy = y + ROW_H / 2.;
    let label = d.t(&format!("settings.{key}"));
    let title_w = setting_title(ui, x, cy, key, icon, &label);
    let open = d.open_sections.contains(key);
    let chevron = if open {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    ui.p.icon(chevron, Rect::new(x + w - 14., cy - 7., 14., 14.), t.muted);
    let style = ui.style().with_size(13.);
    let max = (w - title_w - 40.).max(20.);
    let vl = ui.p.layout(value, style, Some(max));
    ui.p.draw_text(&vl, x + w - 24. - vl.width, cy - vl.height / 2., t.ink2);
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r.inset(0., 4., 0., 4.), 6.);
    if ui.clicked(id) {
        if open {
            d.open_sections.remove(key);
        } else {
            d.open_sections.insert(key);
        }
    }
    (ROW_H, d.open_sections.contains(key))
}

fn second_card(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    let bx = x + 44.;
    let bw = w - 44.;
    // The engine.
    let engine_name = d
        .engine
        .as_ref()
        .map(|e| e.engine_name.clone())
        .unwrap_or_default();
    let (h, open) = section(d, ui, x, y, w, "engine", Icon::Engine, &engine_name);
    y += h;
    if open {
        y += engine_body(d, ui, bx, y, bw) + 18.;
    }
    if d.connection.local {
        separator(ui, x, y, w);
        let (h, open) = section(d, ui, x, y, w, "network", Icon::Networks, "");
        y += h;
        if open {
            y += crate::views::network_settings::body(d, ui, bx, y, bw) + 18.;
        }
    }
    if crate::views::registry_proxy::shown(d) {
        separator(ui, x, y, w);
        let value = crate::views::registry_proxy::summary(d);
        let (h, open) = section(d, ui, x, y, w, "registryProxy", Icon::Globe, &value);
        y += h;
        if open {
            y += crate::views::registry_proxy::body(d, ui, bx, y, bw) + 18.;
        } else {
            crate::views::registry_proxy::closed(d);
        }
    }
    // The VM, for podbag on macOS and Windows.
    if let Some(vm) = d.vm.clone().filter(|_| d.has_vm()) {
        separator(ui, x, y, w);
        let state = d.t(&format!("vm.state.{}", vm_state_key(&vm.state)));
        let (h, open) = section(d, ui, x, y, w, "vm", Icon::Vm, &state);
        y += h;
        if open {
            y += vm_body(d, ui, bx, y, bw, &vm) + 18.;
        }
    }
    // Synchronized shares, for podbag's VM on a Mac.
    if crate::views::sync_shares::shown(d) {
        separator(ui, x, y, w);
        let (h, open) = section(d, ui, x, y, w, "syncShares", Icon::Volumes, "");
        y += h;
        if open {
            y += crate::views::sync_shares::body(d, ui, bx, y, bw) + 18.;
        } else {
            crate::views::sync_shares::closed(d);
        }
    }
    // Updates: the versions, a check, and Update.
    separator(ui, x, y, w);
    let value = crate::views::updates::value(d);
    let (h, open) = section(d, ui, x, y, w, "updates", Icon::Update, &value);
    y += h;
    if open {
        y += crate::views::updates::body(d, ui, bx, y, bw) + 4.;
    }
    // The terminal command, for an app with a podbag of its own.
    if let Some(value) = crate::views::notices::value(d) {
        separator(ui, x, y, w);
        let (h, open) = section(d, ui, x, y, w, "terminalCommand", Icon::Shell, &value);
        y += h;
        if open {
            y += crate::views::notices::body(d, ui, bx, y, bw) + 10.;
        }
    }
    // About.
    separator(ui, x, y, w);
    let (h, open) = section(d, ui, x, y, w, "about", Icon::Inspect, "");
    y += h;
    if open {
        y += ui.paragraph(bx, y, bw, &d.t("settings.aboutText"), ui.style(), t.ink2) + 18.;
    }
    y - y0
}

fn engine_body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    // The engine picker: podbag, Docker, Podman, Kiri, Docker contexts, a socket.
    let label = d.t("settings.engine");
    ui.p.label(Rect::new(x, y, 120., 34.), &label, ui.style(), t.ink2);
    let name = d.connection.name.clone();
    let mut values: Vec<(String, String)> = vec![
        ("podbag".into(), "podbag".into()),
        ("docker".into(), "Docker".into()),
        ("podman".into(), "Podman".into()),
        ("kiri".into(), "Kiri".into()),
    ];
    let group = d.t("engine.contexts");
    let contexts: Vec<(String, String)> = d
        .contexts
        .iter()
        .map(|c| (format!("context:{c}"), c.clone()))
        .collect();
    if !values.iter().chain(&contexts).any(|(v, _)| *v == name) {
        values.push((name.clone(), name.clone()));
    }
    let windows = cfg!(windows) || d.engine.as_ref().is_some_and(|e| e.platform == "windows");
    let socket = d.t("engine.socket");
    let mut items: Vec<Item> = values
        .iter()
        .map(|(_, l)| Item::Option(l.as_str()))
        .collect();
    let mut keys: Vec<Option<&str>> = values.iter().map(|(v, _)| Some(v.as_str())).collect();
    if !contexts.is_empty() {
        items.push(Item::Group(&group));
        keys.push(None);
        for (v, l) in &contexts {
            items.push(Item::Option(l.as_str()));
            keys.push(Some(v.as_str()));
        }
    }
    if !windows {
        items.push(Item::Option(&socket));
        keys.push(Some("custom"));
    }
    let sel = keys
        .iter()
        .position(|k| *k == Some(name.as_str()))
        .unwrap_or(0);
    let sw = (w - 134.).min(300.);
    if let Some(i) = select(
        ui,
        ENGINE,
        Rect::new(x + 134., y, sw, 34.),
        &items,
        sel,
        true,
    ) && let Some(next) = keys[i]
        && next != name
    {
        if next == "custom" {
            d.ask_socket();
        } else if let Err(e) = d.switch_engine(next) {
            d.toasts.push(e, true);
        }
    }
    y += 42.;
    y += ui.paragraph(
        x,
        y,
        w,
        &d.t("settings.engineNote"),
        ui.style().with_size(12.5),
        t.muted,
    ) + 12.;
    let e = d.engine.clone();
    let rows = vec![
        field(
            d.t("settings.socket"),
            e.as_ref().map(|e| e.endpoint.clone()).unwrap_or_default(),
            true,
        ),
        field(
            d.t("settings.version"),
            e.as_ref()
                .and_then(|e| e.version.clone())
                .unwrap_or_default(),
            false,
        ),
    ];
    let none = d.t("detail.none");
    y += fields(ui, Id::of("engine-fields"), x, y, w, &rows, &none);
    // Where the docker command goes.
    if let Some(cmd) = d.docker.clone() {
        separator(ui, x, y + 2., w);
        y += 14.;
        let label = d.t("docker.command");
        let style = ui.style();
        let lw = ui.p.layout(&label, style, None).width;
        ui.p.label(Rect::new(x, y, lw + 2., 30.), &label, style, t.ink2);
        if cmd.installed {
            ui.tooltip(
                Id::of("docker-note"),
                Rect::new(x, y, lw, 30.),
                &d.t("docker.note"),
            );
        }
        let mut bx = x + lw + 12.;
        if !cmd.installed {
            let text = d.t("docker.missing");
            ui.label(Rect::new(bx, y, w - lw - 12., 30.), &text, style, t.ink2);
        } else {
            let route = d.docker_route();
            let (fg, bg) = if cmd.is_podbag && d.docker_switching.is_none() {
                (t.green, t.green_soft)
            } else {
                (t.ink2, t.gray_soft)
            };
            bx += pill(ui, bx, y + 15., &route, fg, bg, w - lw - 120.) + 8.;
            if cmd.from_env {
                let env_style = ui.style().with_size(12.5).weight(600);
                let ew = ui.p.layout("DOCKER_HOST", env_style, None).width + 22.;
                let er = Rect::new(bx, y + 4., ew, 22.);
                ui.p.icon(
                    Icon::Warning.id(),
                    Rect::new(er.x, er.y + 3., 16., 16.),
                    t.amber,
                );
                ui.p.label(er.inset(20., 0., 0., 0.), "DOCKER_HOST", env_style, t.amber);
                ui.tooltip(Id::of("docker-env"), er, &d.t("docker.env"));
                bx += ew + 8.;
            }
            if let Some((icon, to, label)) = switch_button(d, &cmd) {
                let br = Rect::new(bx, y, 30., 30.);
                if button::icon_button(
                    ui,
                    Id::of("docker-switch"),
                    br,
                    icon.id(),
                    &label,
                    IconKind::Accent,
                    d.docker_switching.is_none(),
                ) {
                    d.switch_docker(to);
                }
            }
        }
        y += 30.;
    }
    y += crate::views::compression::setting(d, ui, x, y, w);
    y - y0
}

/// The switch button of the docker command: (icon, `use` or `back`, label), if one
/// shows.
pub fn switch_button(
    d: &Desktop,
    cmd: &podbag_desktop_core::DockerCommand,
) -> Option<(Icon, &'static str, String)> {
    if cmd.is_podbag {
        let prev = cmd.previous.clone()?;
        Some((Icon::Undo, "back", d.tv("docker.back", &[("name", &prev)])))
    } else {
        Some((Icon::Switch, "use", d.t("docker.use")))
    }
}

/// Ask before switching the docker command (the status bar's button).
pub fn ask_switch(d: &mut Desktop, cmd: &podbag_desktop_core::DockerCommand) {
    let Some((_, to, label)) = switch_button(d, cmd) else {
        return;
    };
    let question = if to == "use" {
        d.tv("docker.askUse", &[("name", &cmd.name)])
    } else {
        let prev = cmd.previous.clone().unwrap_or_default();
        d.tv("docker.askBack", &[("name", &prev)])
    };
    let dialog = Dialog::confirm(&d.tr, question, Some(label), Then::Docker(to));
    d.open_dialog(dialog);
}

pub fn fill_vm_form(d: &mut Desktop) {
    let Some(vm) = &d.vm else { return };
    let next = vm.next.or(vm.in_use).unwrap_or_default();
    let f = &mut d.vm_form;
    f.shares = vm
        .shared_paths
        .as_ref()
        .map(|s| s.next.clone())
        .unwrap_or_default();
    f.cpus
        .set_content(&next.cpus.map(|c| c.to_string()).unwrap_or_default());
    f.memory.set_content(
        &next
            .memory_mib
            .map(|m| trim_float(m as f32 / 1024.))
            .unwrap_or_default(),
    );
    f.idle.set_content(
        &next
            .idle_seconds
            .map(|s| ((s as f32 / 60.).round() as u32).to_string())
            .unwrap_or_default(),
    );
}

fn trim_float(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn vm_body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, vm: &VmStatus) -> f32 {
    let t = ui.theme;
    let lang = d.lang();
    let mut y = y0;
    let running = vm.state == "running";
    let stoppable = running || vm.state == "standby" || vm.state == "starting";
    // The state.
    let style = ui.style();
    ui.p.label(
        Rect::new(x, y, 160., 22.),
        &d.t("settings.vmState"),
        style,
        t.ink2,
    );
    let (fg, bg) = match vm_state_key(&vm.state) {
        "running" => (t.green, t.green_soft),
        "starting" => (t.amber, t.amber_soft),
        _ => (t.ink2, t.gray_soft),
    };
    let state = d.t(&format!("vm.state.{}", vm_state_key(&vm.state)));
    let label_w = 132f32;
    pill(ui, x + label_w, y + 11., &state, fg, bg, w - label_w);
    y += 32.;
    let mut rows = Vec::new();
    if let Some(u) = vm.in_use {
        let idle = match u.idle_seconds {
            Some(s) if s > 0 => d.tv(
                "settings.idleAfter",
                &[("time", &fmt::duration(Some(s), lang))],
            ),
            _ => d.t("settings.idleNever"),
        };
        let cpus = d.tr.tn("settings.cpusValue", u.cpus.unwrap_or(0) as usize);
        let text = [cpus, fmt::mib(u.memory_mib, lang), idle].join(" · ");
        rows.push(field(d.t("settings.vmInUse"), text, false));
    }
    if vm.uptime_seconds.is_some() {
        rows.push(field(
            d.t("settings.vmUptime"),
            fmt::duration(vm.uptime_seconds, lang),
            false,
        ));
    }
    if let Some(disk) = &vm.disk {
        let text = d.tv(
            "settings.vmDiskValue",
            &[
                ("used", &fmt::size(disk.allocated_bytes, lang)),
                ("size", &fmt::size(disk.size_bytes, lang)),
            ],
        );
        rows.push(field(d.t("settings.vmDisk"), text, false));
    }
    let none = d.t("detail.none");
    y += fields(ui, Id::of("vm-fields"), x, y, w, &rows, &none) + 4.;
    if let Some(e) = d.vm_error.clone() {
        y += ui.paragraph(x, y, w, &e, ui.style(), t.red) + 8.;
    }
    // Start, stop, restart.
    let mut bx = x;
    for (action, icon, key, enabled, primary) in [
        (
            "start",
            Icon::Start,
            "action.startVm",
            !stoppable && vm.state != "not set up",
            !stoppable,
        ),
        ("stop", Icon::Stop, "action.stopVm", stoppable, false),
        ("restart", Icon::Restart, "action.restartVm", running, false),
    ] {
        let label = d.t(key);
        let bw = button::width(ui, &label, Some(icon.id()), false);
        let kind = if primary {
            ButtonKind::Primary
        } else {
            ButtonKind::Normal
        };
        if button::button(
            ui,
            Id::of(("vm-settings", action)),
            Rect::new(bx, y, bw, 32.),
            &label,
            Some(icon.id()),
            kind,
            enabled && !d.vm_busy,
        ) {
            d.vm_action(action);
        }
        bx += bw + 8.;
    }
    y += 32.;
    if !vm.resources_editable {
        y += 10.
            + ui.paragraph(
                x,
                y + 10.,
                w,
                &d.t("settings.vmWsl"),
                ui.style().with_size(12.5),
                t.muted,
            );
        return y - y0;
    }
    // The form: CPUs, memory, idle.
    y += 18.;
    separator(ui, x, y, w);
    y += 16.;
    let l = vm.limits.unwrap_or_default();
    let (cpus_min, cpus_max) = (
        l.cpus_min.max(1),
        if l.cpus_max > 0 { l.cpus_max } else { 64 },
    );
    let (mem_min, mem_max) = (
        if l.memory_min_mib > 0 {
            l.memory_min_mib
        } else {
            1024
        },
        if l.memory_max_mib > 0 {
            l.memory_max_mib
        } else {
            65536
        },
    );
    let over = |name: &str| vm.overridden_by.iter().any(|n| n == name);
    let col_w = ((w - 28.) / 3.).max(120.);
    let label_style = ui.style().with_size(13.).weight(600);
    let hint_style = ui.style().with_size(12.5);
    let mut col_h: f32 = 0.;
    let specs = [
        (
            "cpus",
            d.t("settings.cpus"),
            Some((cpus_min as f32, cpus_max as f32, 1.)),
            d.tv(
                "settings.limits",
                &[
                    ("min", &cpus_min.to_string()),
                    ("max", &l.cpus_max.to_string()),
                ],
            ),
            "PODBAG_VM_CPUS",
        ),
        (
            "memory",
            d.t("settings.memory"),
            Some((mem_min as f32 / 1024., mem_max as f32 / 1024., 0.25)),
            d.tv(
                "settings.limits",
                &[
                    ("min", &fmt::mib(Some(mem_min), lang)),
                    ("max", &fmt::mib(Some(l.memory_max_mib), lang)),
                ],
            ),
            "PODBAG_VM_MEMORY",
        ),
        (
            "idle",
            d.t("settings.idle"),
            None,
            d.t("settings.idleHint"),
            "PODBAG_VM_IDLE",
        ),
    ];
    for (i, (name, label, range, hint, env)) in specs.iter().enumerate() {
        let cx = x + i as f32 * (col_w + 14.);
        let mut cy = y;
        cy += ui.paragraph(cx, cy, col_w, label, label_style, t.ink) + 5.;
        let input = match *name {
            "cpus" => &mut d.vm_form.cpus,
            "memory" => &mut d.vm_form.memory,
            _ => &mut d.vm_form.idle,
        };
        if text_field(ui, input, Rect::new(cx, cy, col_w, 34.), None, true) {
            d.vm_form.dirty = true;
        }
        cy += 39.;
        if let Some((min, max, step)) = range {
            let input = if *name == "cpus" {
                &mut d.vm_form.cpus
            } else {
                &mut d.vm_form.memory
            };
            let mut v: f32 = input.content().trim().parse().unwrap_or(*min);
            if slider(
                ui,
                Id::of(("vm-slider", *name)),
                Rect::new(cx, cy, col_w, 20.),
                &mut v,
                *min,
                *max,
                *step,
            ) {
                input.set_content(&trim_float(v));
                d.vm_form.dirty = true;
            }
            cy += 25.;
        }
        cy += ui.paragraph(cx, cy, col_w, hint, hint_style, t.muted) + 3.;
        if over(env) {
            let text = d.tv("settings.vmOverridden", &[("name", env)]);
            cy += ui.paragraph(cx, cy, col_w, &text, hint_style, t.amber);
        }
        col_h = col_h.max(cy - y);
    }
    y += col_h + 8.;
    if vm.shared_paths.is_some() {
        y += 6. + shared_folders(d, ui, x, y + 6., w) + 8.;
    }
    if vm.pending {
        y += ui.paragraph(x, y, w, &d.t("settings.vmPending"), hint_style, t.muted) + 6.;
    }
    if !d.vm_form.error.is_empty() {
        let e = d.vm_form.error.clone();
        y += ui.paragraph(x, y, w, &e, ui.style(), t.red) + 6.;
    }
    y += 8.;
    let label = d.t("settings.apply");
    let bw = button::width(ui, &label, None, false);
    if button::button(
        ui,
        APPLY,
        Rect::new(x, y, bw, 32.),
        &label,
        None,
        ButtonKind::Primary,
        !d.vm_form.applying,
    ) {
        apply_vm(d, vm, (cpus_min, cpus_max), (mem_min, mem_max));
    }
    y + 32. - y0
}

/// The macOS folders the VM shares: a field to add one, the list with Remove, and a
/// hint. Returns the height.
fn shared_folders(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    let label_style = ui.style().with_size(13.).weight(600);
    y += ui.paragraph(x, y, w, &d.t("settings.sharedFolders"), label_style, t.ink) + 5.;
    let add_label = d.t("settings.addSharedFolder");
    let bw = button::width(ui, &add_label, None, false);
    let fw = (w - bw - 8.).max(80.);
    if text_field(
        ui,
        &mut d.vm_form.share,
        Rect::new(x, y, fw, 34.),
        None,
        true,
    ) {
        d.vm_form.share_error.clear();
    }
    let share_id = d.vm_form.share.id;
    let enter = ui.key_for(share_id, NamedKey::Enter);
    if button::button(
        ui,
        ADD_SHARE,
        Rect::new(x + fw + 8., y + 1., bw, 32.),
        &add_label,
        None,
        ButtonKind::Normal,
        true,
    ) || enter
    {
        add_share(d);
    }
    y += 34.;
    if !d.vm_form.share_error.is_empty() {
        let e = d.vm_form.share_error.clone();
        y += 4. + ui.paragraph(x, y + 4., w, &e, ui.style().with_size(12.5), t.red);
    }
    y += 5.;
    // The folders, each with Remove.
    let remove = d.t("settings.removeSharedFolder");
    let rw = button::width(ui, &remove, None, false);
    let mono = podbag_ui::TextStyle::new(12.5).mono().line_height(18.);
    let mut removed = None;
    for (i, path) in d.vm_form.shares.clone().iter().enumerate() {
        let pw = (w - rw - 8.).max(40.);
        let ph = ui.p.layout_wrapped(path, mono, pw).height;
        let h = ph.max(32.);
        ui.paragraph(x, y + (h - ph) / 2., pw, path, mono, t.ink);
        let id = remove_share_id(path);
        let br = Rect::new(x + w - rw, y + (h - 32.) / 2., rw, 32.);
        if button::button(ui, id, br, &remove, None, ButtonKind::Normal, true) {
            removed = Some(i);
        }
        let name = d.tv("settings.removeSharedFolderName", &[("path", path)]);
        ui.tooltip(id.with("tip"), br, &name);
        y += h + 5.;
    }
    if let Some(i) = removed {
        let f = &mut d.vm_form;
        f.shares.remove(i);
        f.dirty = true;
        // The focus goes to the next row's Remove, the one before, or the field.
        let next = f
            .shares
            .get(i)
            .or(i.checked_sub(1).and_then(|j| f.shares.get(j)));
        d.focus_next = Some(next.map_or(f.share.id, |p| remove_share_id(p)));
        ui.repaint();
    }
    y += ui.paragraph(
        x,
        y,
        w,
        &d.t("settings.sharedFoldersHint"),
        ui.style().with_size(12.5),
        t.muted,
    );
    y - y0
}

/// Add the folder in the field: an absolute path, once.
fn add_share(d: &mut Desktop) {
    let path = d.vm_form.share.content().to_string();
    if !path.starts_with('/') {
        d.vm_form.share_error = d.t("settings.sharedFolderAbsolute");
        d.focus_next = Some(d.vm_form.share.id);
        return;
    }
    let f = &mut d.vm_form;
    if !f.shares.contains(&path) {
        f.shares.push(path);
    }
    f.dirty = true;
    f.share.set_content("");
    f.share_error.clear();
    d.focus_next = Some(f.share.id);
}

/// Check the form, then save (asking first when the VM runs).
fn apply_vm(d: &mut Desktop, vm: &VmStatus, cpus: (u32, u32), mem: (u64, u64)) {
    d.vm_form.error.clear();
    let parse = |s: &str| s.trim().parse::<f64>().ok();
    let checks = [
        (
            parse(d.vm_form.cpus.content()),
            cpus.0 as f64,
            cpus.1 as f64,
            "settings.cpus",
            1.,
        ),
        (
            parse(d.vm_form.memory.content()),
            mem.0 as f64 / 1024.,
            mem.1 as f64 / 1024.,
            "settings.memory",
            0.25,
        ),
        (
            parse(d.vm_form.idle.content()),
            0.,
            1440.,
            "settings.idle",
            1.,
        ),
    ];
    for (v, min, max, key, step) in checks {
        let ok = v.is_some_and(|v| {
            let k = (v - min) / step;
            v >= min && v <= max && (k - k.round()).abs() < 1e-6
        });
        if !ok {
            let range = d.tv(
                "settings.limits",
                &[
                    ("min", &trim_float(min as f32)),
                    ("max", &trim_float(max as f32)),
                ],
            );
            d.vm_form.error = format!("{}: {range}", d.t(key));
            return;
        }
    }
    let cpus = parse(d.vm_form.cpus.content()).unwrap_or(0.) as u32;
    let memory_mib = (parse(d.vm_form.memory.content()).unwrap_or(0.) * 1024.).round() as u64;
    let idle_seconds = (parse(d.vm_form.idle.content()).unwrap_or(0.) * 60.).round() as u64;
    let next = vm.next.unwrap_or_default();
    let mut change = VmSettings::default();
    if Some(cpus) != next.cpus {
        change.cpus = Some(cpus);
    }
    if Some(memory_mib) != next.memory_mib {
        change.memory_mib = Some(memory_mib);
    }
    if Some(idle_seconds) != next.idle_seconds {
        change.idle_seconds = Some(idle_seconds);
    }
    // The shared folders, compared in order of name.
    let sorted = |paths: &[String]| {
        let mut paths = paths.to_vec();
        paths.sort();
        paths
    };
    let shares = vm.shared_paths.as_ref().map(|s| {
        let paths = sorted(&d.vm_form.shares);
        let in_use = s.in_use.as_deref().map(sorted).unwrap_or_default();
        (paths.clone(), paths != sorted(&s.next), paths != in_use)
    });
    if let Some((paths, true, _)) = &shares {
        change.shared_paths = Some(paths.clone());
    }
    let none = change.cpus.is_none()
        && change.memory_mib.is_none()
        && change.idle_seconds.is_none()
        && change.shared_paths.is_none();
    if none && !vm.pending {
        let text = d.t("settings.vmSame");
        d.toasts.push(text, false);
        return;
    }
    if none {
        change.cpus = Some(cpus); // only to restart with them
    }
    let in_use = vm.in_use.unwrap_or_default();
    let differs = in_use.cpus != Some(cpus)
        || in_use.memory_mib != Some(memory_mib)
        || in_use.idle_seconds != Some(idle_seconds)
        || shares.is_some_and(|(_, _, other)| other);
    if vm.state == "running" && differs {
        let n = vm.containers.unwrap_or(0) as usize;
        let mut text = d.t("settings.vmApplyText");
        if n > 0 {
            text = format!("{text}\n\n{}", d.tr.tn("settings.vmApplyWarn", n));
        }
        let dialog = Dialog::Choose {
            title: d.t("settings.vmApplyTitle"),
            text,
            danger: n > 0,
            change,
        };
        d.open_dialog(dialog);
        return;
    }
    d.save_vm(change);
}
