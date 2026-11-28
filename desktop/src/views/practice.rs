//! The practice pages: the volumes or containers of the practice
//! session a terminal started, with the actions its steps do in the window. Without a
//! session, the commands that start one.

use std::time::{Duration, Instant};

use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::{CursorIcon, Id, Rect};
use serde_json::Value;

use crate::app::{Desktop, Page};
use crate::dialogs::{Dialog, Then};
use crate::icons::Icon;
use crate::views::blocks::{Buttons, card, code_box, heading, small, spawn, text};
use crate::views::training::{self, Flavor, practice_state, start_command, title_of};
use crate::views::{TOOLBAR_H, field, fields, pill};

const POLL: Duration = Duration::from_millis(800);

pub const BACK: Id = Id(0x7072_6163_7401);
pub const GUIDE: Id = Id(0x7072_6163_7402);
pub const DETAIL_BACK: Id = Id(0x7072_6163_7403);
pub const OPEN_WEB: Id = Id(0x7072_6163_7404);
pub const VIEW_PORTS: Id = Id(0x7072_6163_7405);

/// A row of the practice list (`row.with("remove")`, `row.with("stop")`).
pub fn row_id(i: usize) -> Id {
    Id::of(("practice-row", i))
}

#[derive(Default)]
pub struct PracticeState {
    /// The session's state; None (or null) when none runs.
    pub state: Option<Value>,
    pub error: String,
    /// The volume shown (its inspect JSON).
    pub volume: Option<Value>,
    /// The container shown (its id).
    pub container: Option<String>,
    pub busy: bool,
    polling: bool,
    poll_at: Option<Instant>,
    /// Polls of an earlier page do not count.
    generation: u64,
    pub scroll: ScrollState,
}

fn shown(d: &Desktop) -> Option<Flavor> {
    match d.page {
        Page::LabVolumes => Some(Flavor::Volumes),
        Page::LabContainers => Some(Flavor::Containers),
        _ => None,
    }
}

fn default_module(flavor: Flavor) -> &'static str {
    if flavor == Flavor::Volumes {
        "volume-basics"
    } else {
        "container-basics"
    }
}

/// The state, when a session runs.
fn state(d: &Desktop) -> Option<&Value> {
    d.practice.state.as_ref().filter(|s| !s.is_null())
}

fn session(d: &Desktop) -> Option<String> {
    state(d).and_then(|s| s["session"].as_str().map(str::to_string))
}

fn is_course(d: &Desktop) -> bool {
    state(d).is_some_and(|s| s["module"].is_string())
}

pub fn entered(d: &mut Desktop) {
    let generation = d.practice.generation + 1;
    d.practice = PracticeState {
        generation,
        ..PracticeState::default()
    };
    poll(d);
}

/// A course session first, else the lesson's.
pub fn poll(d: &mut Desktop) {
    let Some(flavor) = shown(d) else {
        d.practice.poll_at = None;
        return;
    };
    if d.practice.polling {
        return;
    }
    d.practice.polling = true;
    let generation = d.practice.generation;
    spawn(
        d,
        move |api| -> Result<Value, String> {
            let course = api.practice("status", None, None, false, true)?;
            if !course.is_null() {
                return Ok(course);
            }
            practice_state(api, flavor, "")
        },
        move |d, r| {
            if generation != d.practice.generation {
                return;
            }
            let p = &mut d.practice;
            p.polling = false;
            match r {
                Ok(next) => {
                    let same = next["session"]
                        == p.state
                            .as_ref()
                            .map_or(Value::Null, |s| s["session"].clone());
                    if !same {
                        p.container = None;
                    }
                    let keep = p.volume.as_ref().is_some_and(|v| {
                        same && next["volumes"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|x| x["Name"] == v["Name"]))
                    });
                    if !keep {
                        p.volume = None;
                    }
                    p.state = Some(next);
                    p.error.clear();
                }
                Err(e) => {
                    p.error = e;
                    p.state = None;
                }
            }
            p.poll_at = Some(Instant::now() + POLL);
        },
    );
}

pub fn tick(d: &mut Desktop, now: Instant) -> bool {
    if shown(d).is_some() && d.practice.poll_at.is_some_and(|t| t <= now) {
        d.practice.poll_at = None;
        poll(d);
        return true;
    }
    false
}

pub fn next_due(d: &Desktop) -> Option<Instant> {
    shown(d).and(d.practice.poll_at)
}

/// Run a practice action on `name` (a volume name or a container id) in `session`.
pub fn act(d: &mut Desktop, action: &'static str, name: String, session: String) {
    let Some(flavor) = shown(d) else { return };
    if d.practice.busy || self::session(d).as_deref() != Some(session.as_str()) {
        return;
    }
    d.practice.busy = true;
    let course = is_course(d);
    let generation = d.practice.generation;
    spawn(
        d,
        move |api| {
            api.practice(
                action,
                Some(&session),
                Some(&name),
                flavor == Flavor::Containers && !course,
                course,
            )
        },
        move |d, r| {
            d.practice.busy = false;
            if generation != d.practice.generation {
                return;
            }
            match r {
                Ok(next) if flavor == Flavor::Volumes => {
                    d.practice.volume = next.get("detail").filter(|v| v.is_object()).cloned();
                    d.practice.state = Some(next);
                }
                Ok(_) => {}
                Err(e) => d.toasts.push(e, true),
            }
            d.practice.polling = false;
            poll(d);
        },
    );
}

fn ask_remove(d: &mut Desktop, name: String, session: String) {
    let text = d.tv("confirm.removeVolume", &[("name", &name)]);
    let dialog = Dialog::confirm(&d.tr, text, None, Then::PracticeRemove { name, session });
    d.open_dialog(dialog);
}

fn container_name(c: &Value) -> String {
    c["Names"][0]
        .as_str()
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| {
            c["Id"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(12)
                .collect()
        })
}

// ---- the page ----

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let Some(flavor) = shown(d) else { return };
    let t = ui.theme;
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    ui.p.border_bottom(bar, t.line);
    let label = d.t("nav.tutorials");
    if button::icon_button(
        ui,
        BACK,
        Rect::new(bar.x + 14., bar.y + 16., 30., 30.),
        Icon::Back.id(),
        &label,
        IconKind::Plain,
        true,
    ) {
        training::show(d, None);
        return;
    }
    let title = d.t(if flavor == Flavor::Volumes {
        "practice.volumes"
    } else {
        "training.containers"
    });
    let module = state(d)
        .and_then(|s| s["module"].as_str().map(str::to_string))
        .unwrap_or_else(|| default_module(flavor).to_string());
    let guide = d.t("training.guide");
    let gw = button::width(ui, &guide, Some(Icon::Globe.id()), false);
    let gr = Rect::new(bar.right() - 20. - gw, bar.y + 15., gw, 32.);
    let style = ui.style().with_size(18.).weight(700);
    ui.p.label(
        Rect::new(bar.x + 52., bar.y, gr.x - bar.x - 64., bar.h - 1.),
        &title,
        style,
        t.ink,
    );
    if button::button(
        ui,
        GUIDE,
        gr,
        &guide,
        Some(Icon::Globe.id()),
        ButtonKind::Normal,
        true,
    ) {
        let lang = d.lang();
        if let Err(e) = d.api.open_training_guide(&module, lang) {
            d.toasts.push(e, true);
        }
    }
    let mut scroll = std::mem::take(&mut d.practice.scroll);
    scroll_view(ui, Id::of("practice-scroll"), r, &mut scroll, |ui, area| {
        let x = area.x + 20.;
        let w = area.w - 40.;
        let mut y = area.y + 20.;
        y += match state(d).cloned() {
            None => start_options(d, ui, x, y, w.min(680.), flavor),
            Some(s) if flavor == Flavor::Volumes => volumes(d, ui, x, y, w, &s),
            Some(s) => containers(d, ui, x, y, w, &s),
        };
        if !d.practice.error.is_empty() {
            let e = d.practice.error.clone();
            y += small(ui, x, y + 8., w, &e, t.red) + 8.;
        }
        y + 20. - area.y
    });
    d.practice.scroll = scroll;
}

/// The commands that start a session.
fn start_options(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, flavor: Flavor) -> f32 {
    let t = ui.theme;
    let lang = d.lang();
    let mut y = y0;
    y += text(ui, x, y, w, &d.t("training.terminalStart"), t.ink2) + 12.;
    let records: Vec<_> = d
        .training
        .records
        .iter()
        .filter(|r| match flavor {
            Flavor::Volumes => r.id == "volume-basics" || r.uses_volumes(),
            _ => r.id == "container-basics" || !r.steps.is_empty(),
        })
        .cloned()
        .collect();
    for r in records {
        let title = title_of(
            &r.words
                .get(lang)
                .or_else(|| r.words.get("en"))
                .cloned()
                .unwrap_or_default()
                .0,
        );
        y += heading(ui, x, y, w, &title, 16.) + 8.;
        y += code_box(
            d,
            ui,
            Id::of(("practice-start", &r.id)),
            x,
            y,
            w,
            &start_command(&r.id),
        ) + 18.;
    }
    y - y0
}

/// A list row: the columns' texts from the left, an optional chip, and the action.
#[allow(clippy::too_many_arguments)]
fn list_row(
    ui: &mut Ui,
    id: Id,
    r: Rect,
    cols: &[(String, f32, bool)],
    state: Option<(&str, String)>,
    action: Option<(Icon, String, bool, bool)>,
) -> (bool, bool) {
    let t = ui.theme;
    if ui.hovered(id) {
        ui.p.fill(r, t.hover);
    }
    ui.p.fill(Rect::new(r.x, r.bottom() - 1., r.w, 1.), t.line);
    let mut x = r.x + 12.;
    let style = ui.style();
    let mono = ui.style().with_size(12.5).mono();
    let avail = r.w - 24. - 40.;
    for (text, frac, is_mono) in cols {
        let w = avail * frac;
        ui.label(
            Rect::new(x, r.y, w - 12., r.h),
            text,
            if *is_mono { mono } else { style },
            if x == r.x + 12. { t.ink } else { t.ink2 },
        );
        x += w;
    }
    if let Some((s, label)) = state {
        let (fg, bg) = if s == "running" {
            (t.green, t.green_soft)
        } else {
            (t.ink2, t.gray_soft)
        };
        pill(ui, x, r.y + r.h / 2., &label, fg, bg, avail * 0.2 - 8.);
    }
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r, 6.);
    let mut acted = false;
    if let Some((icon, label, enabled, danger)) = action {
        let b = Rect::new(r.right() - 40., r.y + (r.h - 30.) / 2., 30., 30.);
        let kind = if danger {
            IconKind::Danger
        } else {
            IconKind::Plain
        };
        acted = button::icon_button(ui, id.with("action"), b, icon.id(), &label, kind, enabled);
    }
    (ui.clicked(id) && !acted, acted)
}

fn volumes(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, s: &Value) -> f32 {
    let t = ui.theme;
    let session = s["session"].as_str().unwrap_or_default().to_string();
    let course = s["module"].is_string();
    let stage = s["stage"].as_str().unwrap_or_default();
    let can_remove = |name: &str| {
        if course {
            s["remove_volume"] == name
        } else {
            name != "my-api" || stage == "remove"
        }
    };
    let remove_label = d.t("action.remove");
    let mut y = y0;
    if let Some(v) = d.practice.volume.clone() {
        let name = v["Name"].as_str().unwrap_or_default().to_string();
        let back = d.t("action.back");
        if button::icon_button(
            ui,
            DETAIL_BACK,
            Rect::new(x, y, 30., 30.),
            Icon::Back.id(),
            &back,
            IconKind::Plain,
            true,
        ) {
            d.practice.volume = None;
            return 0.;
        }
        heading(ui, x + 40., y + 3., w - 90., &name, 16.);
        if button::icon_button(
            ui,
            Id::of("practice-detail-remove"),
            Rect::new(x + w - 30., y, 30., 30.),
            Icon::Remove.id(),
            &remove_label,
            IconKind::Danger,
            can_remove(&name),
        ) {
            ask_remove(d, name.clone(), session.clone());
        }
        y += 46.;
        let rows = vec![
            field(d.t("col.name"), name.clone(), false),
            field(
                d.t("col.driver"),
                v["Driver"].as_str().unwrap_or_default(),
                false,
            ),
            field(
                d.t("detail.path"),
                v["Mountpoint"].as_str().unwrap_or_default(),
                true,
            ),
            field(
                d.t("col.created"),
                v["CreatedAt"].as_str().unwrap_or_default(),
                false,
            ),
        ];
        let none = d.t("detail.none");
        y += card(
            ui,
            Id::of("practice-volume"),
            x,
            y,
            w.min(780.),
            |ui, x, y, w| fields(ui, Id::of("practice-volume-fields"), x, y, w, &rows, &none),
        );
        return y - y0;
    }
    let list = s["volumes"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        y += text(ui, x, y, w, &d.t("empty.volumes.title"), t.muted) + 8.;
        return y - y0;
    }
    let style = ui.style().with_size(12.).weight(600);
    ui.p.label(
        Rect::new(x + 12., y, w * 0.5, 24.),
        &d.t("col.name"),
        style,
        t.muted,
    );
    ui.p.label(
        Rect::new(x + 12. + (w - 64.) * 0.5, y, w * 0.3, 24.),
        &d.t("col.driver"),
        style,
        t.muted,
    );
    y += 28.;
    for (i, v) in list.iter().enumerate() {
        let name = v["Name"].as_str().unwrap_or_default().to_string();
        let cols = [
            (name.clone(), 0.5, false),
            (
                v["Driver"].as_str().unwrap_or_default().to_string(),
                0.5,
                false,
            ),
        ];
        let action = Some((Icon::Remove, remove_label.clone(), can_remove(&name), true));
        let (open, acted) = list_row(ui, row_id(i), Rect::new(x, y, w, 44.), &cols, None, action);
        y += 44.;
        if acted {
            ask_remove(d, name, session.clone());
        } else if open {
            act(d, "inspect", name, session.clone());
        }
    }
    y - y0
}

fn containers(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, s: &Value) -> f32 {
    let t = ui.theme;
    let session = s["session"].as_str().unwrap_or_default().to_string();
    let course = s["module"].is_string();
    let stage = s["stage"].as_str().unwrap_or_default();
    let can_stop = |c: &Value| {
        if course {
            s["stop"] == container_name(c).as_str()
        } else {
            stage == "stop"
        }
    };
    let stop_label = d.t("action.stop");
    let list = s["containers"].as_array().cloned().unwrap_or_default();
    let mut y = y0;
    let shown = d
        .practice
        .container
        .clone()
        .and_then(|id| list.iter().find(|c| c["Id"] == id.as_str()).cloned());
    if let Some(c) = shown {
        let name = container_name(&c);
        let id = c["Id"].as_str().unwrap_or_default().to_string();
        let back = d.t("action.back");
        if button::icon_button(
            ui,
            DETAIL_BACK,
            Rect::new(x, y, 30., 30.),
            Icon::Back.id(),
            &back,
            IconKind::Plain,
            true,
        ) {
            d.practice.container = None;
            return 0.;
        }
        heading(ui, x + 40., y + 3., w - 90., &name, 16.);
        if can_stop(&c)
            && button::icon_button(
                ui,
                Id::of("practice-detail-stop"),
                Rect::new(x + w - 30., y, 30., 30.),
                Icon::Stop.id(),
                &stop_label,
                IconKind::Plain,
                !d.practice.busy,
            )
        {
            act(d, "stop", id.clone(), session.clone());
        }
        y += 46.;
        let state_text = c["State"].as_str().unwrap_or_default().to_string();
        let rows = vec![
            field(d.t("col.name"), name.clone(), false),
            field(
                d.t("col.image"),
                c["Image"].as_str().unwrap_or_default(),
                true,
            ),
            field(d.t("col.state"), d.t(&format!("state.{state_text}")), false),
            field(
                d.t("detail.status"),
                c["Status"].as_str().unwrap_or_default(),
                false,
            ),
            field(d.t("col.id"), id.chars().take(12).collect::<String>(), true),
        ];
        let none = d.t("detail.none");
        let cw = w.min(780.);
        y += card(ui, Id::of("practice-container"), x, y, cw, |ui, x, y, w| {
            fields(
                ui,
                Id::of("practice-container-fields"),
                x,
                y,
                w,
                &rows,
                &none,
            )
        }) + 16.;
        let web = s["web"].clone();
        if web["container"] == name.as_str() {
            y += card(ui, Id::of("practice-ports"), x, y, cw, |ui, x, y0, w| {
                let mut y = y0;
                y += heading(ui, x, y, w, &d.t("col.ports"), 16.) + 10.;
                let mut row = Buttons::new(x, y, w);
                if web["ready"] == true {
                    let port = web["port"].as_u64().unwrap_or_default();
                    y += text(ui, x, y, w, &format!("127.0.0.1:{port} → 8080/tcp"), t.ink) + 8.;
                    let url = web["url"].as_str().unwrap_or_default().to_string();
                    y += code_box(d, ui, Id::of("practice-url"), x, y, w, &url) + 12.;
                    row = Buttons::new(x, y, w);
                    if row.add(
                        ui,
                        OPEN_WEB,
                        &d.t("training.openBrowser"),
                        Some(Icon::Globe),
                        ButtonKind::Primary,
                        !d.practice.busy,
                    ) {
                        act(d, "open-web", id.clone(), session.clone());
                    }
                } else if row.add(
                    ui,
                    VIEW_PORTS,
                    &d.t("training.viewPorts"),
                    None,
                    ButtonKind::Normal,
                    !d.practice.busy,
                ) {
                    act(d, "ports", id.clone(), session.clone());
                }
                y + row.height(y) - y0
            });
        }
        return y - y0;
    }
    if list.is_empty() {
        y += text(ui, x, y, w, &d.t("empty.containers.title"), t.muted) + 8.;
        return y - y0;
    }
    let style = ui.style().with_size(12.).weight(600);
    let avail = w - 64.;
    for (label, at) in [("col.name", 0.), ("col.image", 0.4), ("col.state", 0.8)] {
        ui.p.label(
            Rect::new(x + 12. + avail * at, y, avail * 0.3, 24.),
            &d.t(label),
            style,
            t.muted,
        );
    }
    y += 28.;
    for (i, c) in list.iter().enumerate() {
        let id = c["Id"].as_str().unwrap_or_default().to_string();
        let state_key = c["State"].as_str().unwrap_or_default().to_string();
        let cols = [
            (container_name(c), 0.4, false),
            (
                c["Image"].as_str().unwrap_or_default().to_string(),
                0.4,
                true,
            ),
        ];
        let action = can_stop(c).then(|| (Icon::Stop, stop_label.clone(), !d.practice.busy, false));
        let label = d.t(&format!("state.{state_key}"));
        let (open, acted) = list_row(
            ui,
            row_id(i),
            Rect::new(x, y, w, 44.),
            &cols,
            Some((state_key.as_str(), label)),
            action,
        );
        y += 44.;
        if acted {
            act(d, "stop", id, session.clone());
        } else if open {
            // Opening the failed container is the
            // step that asks for it.
            d.practice.container = Some(id.clone());
            if s["inspect_container"] == container_name(c).as_str() {
                act(d, "inspect-failed", id, session.clone());
            }
        }
    }
    y - y0
}
