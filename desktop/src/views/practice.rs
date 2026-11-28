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
use crate::views::blocks::{Buttons, card, code_box, heading, pre, small, spawn, text};
use crate::views::training::{self, Flavor, StepUi, practice_state, start_command, title_of};
use crate::views::{TOOLBAR_H, field, fields, pill};

const POLL: Duration = Duration::from_millis(800);

pub const BACK: Id = Id(0x7072_6163_7401);
pub const GUIDE: Id = Id(0x7072_6163_7402);
pub const DETAIL_BACK: Id = Id(0x7072_6163_7403);
pub const OPEN_WEB: Id = Id(0x7072_6163_7404);
pub const VIEW_PORTS: Id = Id(0x7072_6163_7405);
/// The toolbar button of a module `ui` step whose action has no row on the page.
pub const STEP: Id = Id(0x7072_6163_7406);
/// The action in the header of the container or volume shown.
pub const DETAIL_ACTION: Id = Id(0x7072_6163_7407);

/// The row buttons of a module `ui` step: on the row its `name` names.
const ROW_STEPS: [(&str, Icon, &str); 7] = [
    ("start", Icon::Start, "action.start"),
    ("stop", Icon::Stop, "action.stop"),
    ("restart", Icon::Restart, "action.restart"),
    ("pause", Icon::Pause, "action.pause"),
    ("resume", Icon::Start, "action.resume"),
    ("remove", Icon::Remove, "action.remove"),
    ("logs", Icon::Logs, "action.logs"),
];

/// The toolbar buttons of a module `ui` step.
const TOOLBAR_STEPS: [(&str, Icon); 6] = [
    ("pull", Icon::Pull),
    ("remove-image", Icon::Remove),
    ("create-volume", Icon::Volumes),
    ("remove-network", Icon::Remove),
    ("prune", Icon::Prune),
    ("inspect", Icon::Inspect),
];

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
    /// The text of a logs step.
    pub log: Option<StepLog>,
    /// The result of the toolbar's inspect step.
    pub inspected: Option<Inspected>,
    pub busy: bool,
    polling: bool,
    poll_at: Option<Instant>,
    /// Discard replies from a previous page or from before the latest action finished.
    generation: u64,
    pub scroll: ScrollState,
}

impl PracticeState {
    fn finish_action(&mut self, generation: u64) -> bool {
        if self.generation != generation {
            return false;
        }
        self.busy = false;
        // The next poll must read the state after this action.
        self.generation += 1;
        self.polling = false;
        true
    }
}

/// The text of a logs step: shown only under the container `id`, and dropped when the
/// step moves on from `stage` or the container is gone.
#[derive(Clone, Debug, PartialEq)]
pub struct StepLog {
    pub id: String,
    pub stage: String,
    pub text: String,
}

/// The result of the toolbar's inspect step (a network or a volume): dropped when the
/// step moves on from `stage`.
#[derive(Clone, Debug, PartialEq)]
pub struct Inspected {
    pub stage: String,
    pub detail: Value,
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

/// The current step's module `ui`, in a course session.
fn step_ui(s: &Value) -> Option<StepUi> {
    s["module"]
        .is_string()
        .then(|| StepUi::of(&s["ui"]))
        .flatten()
}

/// The step's row button on the row named `name`: its action, icon and label.
fn row_step(s: &Value, name: &str) -> Option<(StepUi, Icon, &'static str)> {
    let ui = step_ui(s).filter(|ui| ui.name == name)?;
    let (_, icon, label) = ROW_STEPS.iter().find(|(a, ..)| *a == ui.action)?;
    Some((ui, *icon, label))
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
                        p.log = None;
                        p.inspected = None;
                    }
                    let stage = next["stage"].as_str().unwrap_or_default();
                    if p.inspected.as_ref().is_some_and(|i| i.stage != stage) {
                        p.inspected = None;
                    }
                    let listed = |id: &str| {
                        next["containers"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|c| c["Id"] == id))
                    };
                    if p.log
                        .as_ref()
                        .is_some_and(|l| l.stage != stage || !listed(&l.id))
                    {
                        p.log = None;
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
            if !d.practice.finish_action(generation) {
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
            poll(d);
        },
    );
}

fn ask_remove(d: &mut Desktop, name: String, session: String) {
    let text = d.tv("confirm.removeVolume", &[("name", &name)]);
    let dialog = Dialog::confirm(&d.tr, text, None, Then::PracticeRemove { name, session });
    d.open_dialog(dialog);
}

/// Do the module `ui` step: ask first when it removes something (`running`: the
/// container to remove is up).
fn ask_step(d: &mut Desktop, ui: StepUi, session: String, running: bool) {
    let name = ui.name.as_str();
    let (key, yes) = match ui.action.as_str() {
        "remove" if running => ("confirm.removeRunning", None),
        "remove" => ("confirm.removeContainer", None),
        "remove-image" => ("confirm.removeImage", None),
        "remove-network" => ("confirm.removeNetwork", None),
        "remove-volume" => ("confirm.removeVolume", None),
        "prune" => (
            match name {
                "images" => "confirm.pruneImages",
                "volumes" => "confirm.pruneVolumes",
                "networks" => "confirm.pruneNetworks",
                _ => "confirm.pruneContainers",
            },
            Some(d.t("action.prune")),
        ),
        _ => return step(d, ui.action, ui.name, session),
    };
    let text = d.tv(key, &[("name", name)]);
    let then = Then::PracticeStep {
        action: ui.action,
        name: ui.name,
        session,
    };
    d.open_dialog(Dialog::confirm(&d.tr, text, yes, then));
}

/// Do the module `ui` step `action` on `name` (never an ID) in the course session
/// `session`. A logs step's text and a toolbar inspect's details stay on the page while
/// the session is at the step the reply came back with.
pub fn step(d: &mut Desktop, action: String, name: String, session: String) {
    if shown(d).is_none()
        || d.practice.busy
        || self::session(d).as_deref() != Some(session.as_str())
    {
        return;
    }
    d.practice.busy = true;
    let generation = d.practice.generation;
    let (a, n, s) = (action.clone(), name.clone(), session.clone());
    spawn(
        d,
        move |api| api.practice(&a, Some(&s), Some(&n), false, true),
        move |d, r| {
            if !d.practice.finish_action(generation) {
                return;
            }
            match r {
                Ok(next) if next["session"] == session.as_str() => {
                    let stage = next["stage"].as_str().unwrap_or_default().to_string();
                    let row = next["containers"]
                        .as_array()
                        .and_then(|a| a.iter().find(|c| container_name(c) == name))
                        .and_then(|c| c["Id"].as_str())
                        .map(str::to_string);
                    let p = &mut d.practice;
                    if action == "logs"
                        && let (Some(text), Some(id)) = (next["text"].as_str(), row.clone())
                    {
                        p.log = Some(StepLog {
                            id: id.clone(),
                            stage,
                            text: text.to_string(),
                        });
                        p.container = Some(id);
                    } else if action == "inspect" && row.is_none() && next["detail"].is_object() {
                        p.inspected = Some(Inspected {
                            stage,
                            detail: next["detail"].clone(),
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => d.toasts.push(e, true),
            }
            poll(d);
        },
    );
}

/// The debug `lab step`: the module `ui` step, or the open of the failed container.
pub fn debug_step(d: &mut Desktop) {
    let Some(s) = state(d).cloned() else { return };
    let session = s["session"].as_str().unwrap_or_default().to_string();
    if let Some(u) = step_ui(&s) {
        return step(d, u.action, u.name, session);
    }
    let failed = s["containers"].as_array().and_then(|a| {
        a.iter()
            .find(|c| s["inspect_container"] == container_name(c).as_str())
    });
    if let Some(c) = failed.cloned() {
        open_container(d, &s, &c, session);
    }
}

/// The toolbar button of the module `ui` step, when its action has no row on the page
/// (`rows`: the names of the page's rows). Returns the height.
fn step_button(
    d: &mut Desktop,
    ui: &mut Ui,
    x: f32,
    y: f32,
    w: f32,
    s: &Value,
    rows: &[String],
) -> f32 {
    let Some(step_ui) = step_ui(s) else { return 0. };
    let Some((_, icon)) = TOOLBAR_STEPS.iter().find(|(a, _)| *a == step_ui.action) else {
        return 0.;
    };
    if step_ui.action == "inspect" && rows.contains(&step_ui.name) {
        return 0.;
    }
    let label = d.tv(
        &format!("training.ui.button.{}", step_ui.action),
        &[("name", &step_ui.name)],
    );
    let mut row = Buttons::new(x, y, w);
    let busy = d.practice.busy;
    if row.add(ui, STEP, &label, Some(*icon), ButtonKind::Normal, !busy) {
        let session = s["session"].as_str().unwrap_or_default().to_string();
        ask_step(d, step_ui, session, false);
    }
    row.height(y) + 16.
}

/// The details the toolbar's inspect step showed: a volume or a network. Returns the
/// height.
fn inspected(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let Some(Inspected { detail: v, .. }) = d.practice.inspected.clone() else {
        return 0.;
    };
    let mut y = y0;
    let name = v["Name"].as_str().unwrap_or_default();
    y += heading(ui, x, y, w, name, 16.) + 10.;
    let rows = if v["Mountpoint"].is_string() {
        volume_fields(d, &v)
    } else {
        let ipam = &v["IPAM"]["Config"][0];
        vec![
            field(d.t("col.name"), name, false),
            field(
                d.t("col.id"),
                v["Id"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(12)
                    .collect::<String>(),
                true,
            ),
            field(
                d.t("col.driver"),
                v["Driver"].as_str().unwrap_or_default(),
                false,
            ),
            field(
                d.t("col.subnet"),
                ipam["Subnet"].as_str().unwrap_or_default(),
                true,
            ),
            field(
                d.t("detail.gateway"),
                ipam["Gateway"].as_str().unwrap_or_default(),
                true,
            ),
        ]
    };
    let none = d.t("detail.none");
    y += card(
        ui,
        Id::of("practice-inspected"),
        x,
        y,
        w.min(780.),
        |ui, x, y, w| {
            fields(
                ui,
                Id::of("practice-inspected-fields"),
                x,
                y,
                w,
                &rows,
                &none,
            )
        },
    ) + 16.;
    y - y0
}

fn volume_fields(d: &Desktop, v: &Value) -> Vec<crate::views::Field> {
    vec![
        field(
            d.t("col.name"),
            v["Name"].as_str().unwrap_or_default(),
            false,
        ),
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
    ]
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
    // The module `ui` step that removes the row's volume.
    let step_removes =
        |name: &str| step_ui(s).filter(|u| u.action == "remove-volume" && u.name == name);
    let can_remove = |name: &str| {
        if course {
            s["remove_volume"] == name || step_removes(name).is_some()
        } else {
            name != "my-api" || stage == "remove"
        }
    };
    let remove = |d: &mut Desktop, name: String| match step_removes(&name) {
        Some(u) => ask_step(d, u, session.clone(), false),
        None => ask_remove(d, name, session.clone()),
    };
    let list = s["volumes"].as_array().cloned().unwrap_or_default();
    let rows: Vec<String> = list
        .iter()
        .filter_map(|v| v["Name"].as_str().map(str::to_string))
        .collect();
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
            DETAIL_ACTION,
            Rect::new(x + w - 30., y, 30., 30.),
            Icon::Remove.id(),
            &remove_label,
            IconKind::Danger,
            can_remove(&name),
        ) {
            remove(d, name.clone());
        }
        y += 46.;
        y += step_button(d, ui, x, y, w, s, &rows);
        y += inspected(d, ui, x, y, w);
        let fields_of = volume_fields(d, &v);
        let none = d.t("detail.none");
        y += card(
            ui,
            Id::of("practice-volume"),
            x,
            y,
            w.min(780.),
            |ui, x, y, w| {
                fields(
                    ui,
                    Id::of("practice-volume-fields"),
                    x,
                    y,
                    w,
                    &fields_of,
                    &none,
                )
            },
        );
        return y - y0;
    }
    y += step_button(d, ui, x, y, w, s, &rows);
    y += inspected(d, ui, x, y, w);
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
            remove(d, name);
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
    // The row's action: the module `ui` step on the row it names, else the step's Stop.
    let row_action = |d: &Desktop, c: &Value| {
        if let Some((u, icon, label)) = row_step(s, &container_name(c)) {
            return Some((icon, d.t(label), !d.practice.busy, u.action == "remove"));
        }
        can_stop(c).then(|| (Icon::Stop, stop_label.clone(), !d.practice.busy, false))
    };
    let do_row = |d: &mut Desktop, c: &Value| match row_step(s, &container_name(c)) {
        Some((u, ..)) => {
            let up = matches!(
                c["State"].as_str(),
                Some("running" | "paused" | "restarting")
            );
            ask_step(d, u, session.clone(), up);
        }
        None => {
            let id = c["Id"].as_str().unwrap_or_default().to_string();
            act(d, "stop", id, session.clone());
        }
    };
    let list = s["containers"].as_array().cloned().unwrap_or_default();
    let rows: Vec<String> = list.iter().map(container_name).collect();
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
            d.practice.log = None;
            return 0.;
        }
        heading(ui, x + 40., y + 3., w - 90., &name, 16.);
        if let Some((icon, label, enabled, danger)) = row_action(d, &c) {
            let kind = if danger {
                IconKind::Danger
            } else {
                IconKind::Plain
            };
            let b = Rect::new(x + w - 30., y, 30., 30.);
            if button::icon_button(ui, DETAIL_ACTION, b, icon.id(), &label, kind, enabled) {
                do_row(d, &c);
            }
        }
        y += 46.;
        y += step_button(d, ui, x, y, w, s, &rows);
        y += inspected(d, ui, x, y, w);
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
        if let Some(log) = d.practice.log.clone().filter(|l| l.id == id) {
            y += pre(ui, Id::of("practice-log"), x, y, cw, &log.text) + 16.;
        }
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
    y += step_button(d, ui, x, y, w, s, &rows);
    y += inspected(d, ui, x, y, w);
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
        let state_key = c["State"].as_str().unwrap_or_default().to_string();
        let cols = [
            (container_name(c), 0.4, false),
            (
                c["Image"].as_str().unwrap_or_default().to_string(),
                0.4,
                true,
            ),
        ];
        let action = row_action(d, c);
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
            do_row(d, c);
        } else if open {
            open_container(d, s, c, session.clone());
        }
    }
    y - y0
}

/// Show the container. Opening it is the step that asks for it: the failed container's
/// inspect, or a module `ui` inspect of it.
fn open_container(d: &mut Desktop, s: &Value, c: &Value, session: String) {
    let id = c["Id"].as_str().unwrap_or_default().to_string();
    let name = container_name(c);
    if d.practice.log.as_ref().is_some_and(|l| l.id != id) {
        d.practice.log = None;
    }
    d.practice.container = Some(id.clone());
    if step_ui(s).is_some_and(|u| u.action == "inspect" && u.name == name) {
        step(d, "inspect".into(), name, session);
    } else if s["inspect_container"] == name.as_str() {
        act(d, "inspect-failed", id, session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;
    use crate::jobs::Done;
    use podbag_desktop_core::{Backend, mock};

    #[test]
    fn a_late_poll_keeps_the_log_from_a_newer_action() {
        let (api, events) = Backend::mock(mock::Options::default(), || {});
        let mut d = Desktop::new(api, events, Options::inline());
        d.jobs.drain();
        d.page = Page::LabContainers;
        poll(&mut d);
        let Done::View(old_status) = d.jobs.drain().pop().unwrap() else {
            panic!("expected a practice status reply");
        };
        let generation = d.practice.generation;
        d.practice.busy = true;
        assert!(d.practice.finish_action(generation));
        d.practice.log = Some(StepLog {
            id: "web".into(),
            stage: "read".into(),
            text: "ready".into(),
        });
        old_status(&mut d);
        assert_eq!(d.practice.log.as_ref().unwrap().text, "ready");

        d.practice.busy = true;
        assert!(!d.practice.finish_action(generation));
        assert!(d.practice.busy);
    }
}
