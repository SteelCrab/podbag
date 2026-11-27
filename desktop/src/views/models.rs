//! Models: a local Ollama that the app starts and stops, its models, and a prompt to
//! run one. Opening the page only asks the state: only Start starts the runner, and
//! only Pull downloads.

use podbag_desktop_core::model_runner::{Action, Failure, Reply, Status};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::selectable::selectable_text;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{Id, Point, Rect, TextInput};

use crate::app::Desktop;
use crate::fmt;
use crate::icons::Icon;
use crate::views::blocks::{card, input, spawn};
use crate::views::{TOOLBAR_H, title_bar};

pub const START: Id = Id(0x6d6f_6465_6c01);
pub const STOP: Id = Id(0x6d6f_6465_6c02);
pub const REFRESH: Id = Id(0x6d6f_6465_6c03);
pub const NAME: Id = Id(0x6d6f_6465_6c04);
pub const PULL: Id = Id(0x6d6f_6465_6c05);
pub const MODEL: Id = Id(0x6d6f_6465_6c06);
pub const PROMPT: Id = Id(0x6d6f_6465_6c07);
pub const RUN: Id = Id(0x6d6f_6465_6c08);

/// The Unload button of a model's row.
pub fn unload_id(name: &str) -> Id {
    Id::of(("model-unload", name))
}

/// The errors the page names; others show as `backend`.
const KINDS: [&str; 8] = [
    "missing",
    "start",
    "stopped",
    "modelName",
    "notInstalled",
    "prompt",
    "backend",
    "busy",
];

pub struct ModelsState {
    /// The runner, as last read; None until then, and after it failed or stopped.
    pub status: Option<Status>,
    /// The action that runs (`status`, `start`, …).
    pub pending: Option<&'static str>,
    pub error: Option<Failure>,
    /// Counts the actions; only the result of the last one counts.
    generation: u64,
    pub name: TextInput,
    /// The model to run.
    pub selected: String,
    pub prompt: TextInput,
    pub response: String,
    pub scroll: ScrollState,
}

impl Default for ModelsState {
    fn default() -> Self {
        ModelsState {
            status: None,
            pending: None,
            error: None,
            generation: 0,
            name: input(NAME, "smollm2:135m"),
            selected: String::new(),
            prompt: input(PROMPT, ""),
            response: String::new(),
            scroll: ScrollState::default(),
        }
    }
}

impl ModelsState {
    pub fn running(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.running)
    }

    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
}

/// The page was opened: read the state (nothing starts).
pub fn entered(d: &mut Desktop) {
    act(d, Action::Status);
}

/// Run `action`. While one runs, only Stop runs too; the other's result is then
/// dropped.
pub fn act(d: &mut Desktop, action: Action) {
    let m = &mut d.models;
    if m.pending.is_some() && action != Action::Stop {
        return;
    }
    m.generation += 1;
    let generation = m.generation;
    let name = action.name();
    m.pending = Some(name);
    m.error = None;
    if name == "run" {
        m.response.clear();
    }
    spawn(
        d,
        move |api| {
            let reply = api.models(action)?;
            // After a run, the model is loaded: read the state again.
            match reply {
                Reply::Generation(g) => match api.models(Action::Status)? {
                    Reply::Status(status) => Ok((Some(g.response), status)),
                    Reply::Generation(_) => Err(Failure::new("backend")),
                },
                Reply::Status(status) => Ok((None, status)),
            }
        },
        move |d, result| finished(d, generation, name, result),
    );
}

fn finished(
    d: &mut Desktop,
    generation: u64,
    action: &str,
    result: Result<(Option<String>, Status), Failure>,
) {
    let m = &mut d.models;
    if generation != m.generation {
        return;
    }
    m.pending = None;
    match result {
        Ok((response, status)) => {
            m.status = Some(status);
            if let Some(response) = response {
                m.response = response;
            }
            if action == "pull" {
                m.name.set_content("");
            }
        }
        Err(e) => {
            let e = if KINDS.contains(&e.kind) {
                e
            } else {
                Failure::new("backend")
            };
            if matches!(e.kind, "start" | "stopped") {
                m.status = None;
            }
            m.error = Some(e);
        }
    }
}

fn pull(d: &mut Desktop) {
    let model = d.models.name.content().to_string();
    act(d, Action::Pull { model });
}

fn run(d: &mut Desktop) {
    let (model, prompt) = (
        d.models.selected.clone(),
        d.models.prompt.content().to_string(),
    );
    act(d, Action::Run { model, prompt });
}

pub fn paint(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let mut r = r;
    let bar = r.cut_top(TOOLBAR_H);
    title_bar(ui, bar, &d.t("nav.models"));
    toolbar(d, ui, bar);
    let mut scroll = std::mem::take(&mut d.models.scroll);
    scroll_view(ui, Id::of("models-scroll"), r, &mut scroll, |ui, area| {
        let x = area.x + 20.;
        let w = (area.w - 40.).max(120.);
        let y = area.y + 18.;
        content(d, ui, x, y, w) + 18. + 24.
    });
    d.models.scroll = scroll;
}

/// Refresh (while it runs) and Start or Stop, at the right of the bar.
fn toolbar(d: &mut Desktop, ui: &mut Ui, bar: Rect) {
    let running = d.models.running();
    let pending = d.models.pending;
    let (id, icon, key, kind, enabled) = if running {
        (
            STOP,
            Icon::Stop,
            "models.stop",
            ButtonKind::Normal,
            pending != Some("stop"),
        )
    } else {
        (
            START,
            Icon::Start,
            "models.start",
            ButtonKind::Primary,
            pending.is_none(),
        )
    };
    let label = d.t(key);
    let bw = button::width(ui, &label, Some(icon.id()), false);
    let br = Rect::new(bar.right() - 20. - bw, bar.y + 15., bw, 32.);
    if running {
        let label = d.t("action.refresh");
        if button::icon_button(
            ui,
            REFRESH,
            Rect::new(br.x - 38., bar.y + 16., 30., 30.),
            Icon::Refresh.id(),
            &label,
            IconKind::Plain,
            pending.is_none(),
        ) {
            act(d, Action::Status);
        }
    }
    if button::button(ui, id, br, &label, Some(icon.id()), kind, enabled) {
        act(d, if running { Action::Stop } else { Action::Start });
    }
}

/// The state, the progress and errors, the models and the prompt; returns the height.
fn content(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    let running = d.models.running();
    // The state: a dot, the words, and the version.
    let style = ui.style().weight(600);
    let state = d.t(if running {
        "models.running"
    } else {
        "models.stopped"
    });
    let dot = if running { t.green } else { t.muted };
    ui.p.rounded(Rect::new(x, y + 6.5, 7., 7.), 3.5, dot);
    let l = ui.p.layout(&state, style, Some(w - 13.));
    ui.p.draw_text(&l, x + 13., y, t.ink);
    if let Some(status) = d.models.status.as_ref().filter(|s| s.running) {
        let version = format!("Ollama {}", status.version);
        let vs = ui.style().with_size(13.);
        let vl =
            ui.p.layout(&version, vs, Some((w - l.width - 23.).max(20.)));
        ui.p.draw_text(&vl, x + 23. + l.width, y + 1., t.ink2);
    }
    y += l.height.max(20.) + 14.;
    let hint = ui.style().with_size(13.);
    if !running {
        y += ui.paragraph(x, y, w, &d.t("models.hint"), hint, t.ink2) + 14.;
    }
    if let Some(action) = d.models.pending {
        let text = d.t(&format!("models.progress.{action}"));
        y += ui.paragraph(x, y, w, &text, hint, t.ink2) + 14.;
    }
    if let Some(e) = d.models.error.clone() {
        let text = d.t(&format!("models.error.{}", e.kind));
        y += ui.paragraph(x, y, w, &text, ui.style(), t.red);
        if let Some(detail) = &e.detail {
            y += 4. + ui.paragraph(x, y + 4., w, detail, hint, t.ink2);
        }
        y += 14.;
    }
    let Some(status) = d.models.status.clone().filter(|s| s.running) else {
        return y - y0;
    };
    let busy = d.models.busy();
    y += card(ui, Id::of("models-installed"), x, y, w, |ui, x, y, w| {
        installed(d, ui, x, y, w, &status, busy)
    }) + 14.;
    if !status.models.is_empty() {
        y += card(ui, Id::of("models-generate"), x, y, w, |ui, x, y, w| {
            generate(d, ui, x, y, w, &status, busy)
        }) + 14.;
    }
    y - 14. - y0
}

fn card_title(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let style = ui.style().with_size(15.).weight(700);
    let ink = ui.theme.ink;
    ui.paragraph(x, y, w, text, style, ink) + 14.
}

fn field_label(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let style = ui.style().with_size(13.).weight(600);
    let ink = ui.theme.ink;
    ui.paragraph(x, y, w, text, style, ink) + 7.
}

/// The pull form and the table of models; returns the height.
fn installed(
    d: &mut Desktop,
    ui: &mut Ui,
    x: f32,
    y0: f32,
    w: f32,
    status: &Status,
    busy: bool,
) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    y += card_title(ui, x, y, w, &d.t("models.installed"));
    y += field_label(ui, x, y, w, &d.t("models.registryName"));
    let label = d.t("models.pull");
    let bw = button::width(ui, &label, Some(Icon::Pull.id()), false);
    let fw = (w - bw - 10.).max(80.);
    text_field(
        ui,
        &mut d.models.name,
        Rect::new(x, y, fw, 34.),
        None,
        !busy,
    );
    let enter = !busy && ui.key_for(NAME, NamedKey::Enter);
    if button::button(
        ui,
        PULL,
        Rect::new(x + fw + 10., y + 1., bw, 32.),
        &label,
        Some(Icon::Pull.id()),
        ButtonKind::Normal,
        !busy,
    ) || enter
    {
        pull(d);
    }
    y += 34. + 12.;
    if status.models.is_empty() {
        let hint = ui.style().with_size(13.);
        return y + ui.paragraph(x, y, w, &d.t("models.empty"), hint, t.ink2) - y0;
    }
    // The table: model, size, state, and Unload for a loaded one.
    // Size and state are as wide as their words; the name takes the rest.
    let unload = d.t("models.unload");
    let uw = button::width(ui, &unload, None, true);
    let head = ui.style().with_size(13.).weight(600);
    let cell = ui.style().with_size(13.);
    let mono = podbag_ui::TextStyle::new(13.).mono().line_height(18.);
    let lang = d.lang();
    let widest = |ui: &mut Ui, texts: &[String], style| {
        texts
            .iter()
            .map(|s| ui.p.layout(s, style, None).width)
            .fold(0f32, f32::max)
    };
    let sizes: Vec<String> = status
        .models
        .iter()
        .map(|m| fmt::size(m.size, lang))
        .collect();
    let states = [d.t("models.loaded"), d.t("models.idle")];
    let size_w = widest(ui, &[d.t("col.size")], head).max(widest(ui, &sizes, cell)) + 20.;
    let state_w = widest(ui, &[d.t("detail.status")], head).max(widest(ui, &states, cell)) + 20.;
    let name_w = (w - size_w - state_w - uw - 20.).max(80.);
    let cols = [name_w, size_w];
    for (i, key) in ["models.model", "col.size", "detail.status"]
        .iter()
        .enumerate()
    {
        let cx = x + 10. + cols[..i].iter().sum::<f32>();
        let cw = if i < 2 { cols[i] - 10. } else { state_w - 10. };
        ui.label(Rect::new(cx, y, cw.max(20.), 34.), &d.t(key), head, t.ink2);
    }
    y += 34.;
    for model in &status.models {
        ui.p.fill(Rect::new(x, y, w, 1.), t.line);
        let row = Rect::new(x, y, w, 40.);
        ui.label(
            Rect::new(x + 10., y, cols[0] - 20., 40.),
            &model.name,
            mono,
            t.ink,
        );
        let size = fmt::size(model.size, lang);
        ui.label(
            Rect::new(x + cols[0] + 10., y, cols[1] - 20., 40.),
            &size,
            cell,
            t.ink,
        );
        let state = d.t(if model.loaded {
            "models.loaded"
        } else {
            "models.idle"
        });
        let sx = x + cols[0] + cols[1] + 10.;
        ui.label(
            Rect::new(sx, y, (state_w - 10.).max(20.), 40.),
            &state,
            cell,
            t.ink,
        );
        if model.loaded
            && button::button(
                ui,
                unload_id(&model.name),
                Rect::new(row.right() - 10. - uw, y + 7., uw, 26.),
                &unload,
                None,
                ButtonKind::Normal,
                !busy,
            )
        {
            act(
                d,
                Action::Unload {
                    model: model.name.clone(),
                },
            );
        }
        y += 40.;
    }
    y - y0
}

/// The model, the prompt, Run and the response; returns the height.
fn generate(
    d: &mut Desktop,
    ui: &mut Ui,
    x: f32,
    y0: f32,
    w: f32,
    status: &Status,
    busy: bool,
) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    if !status.models.iter().any(|m| m.name == d.models.selected) {
        d.models.selected = status.models[0].name.clone();
    }
    y += card_title(ui, x, y, w, &d.t("models.generate"));
    y += field_label(ui, x, y, w, &d.t("models.model"));
    let items: Vec<Item> = status
        .models
        .iter()
        .map(|m| Item::Option(m.name.as_str()))
        .collect();
    let sel = status
        .models
        .iter()
        .position(|m| m.name == d.models.selected)
        .unwrap_or(0);
    let mono = podbag_ui::TextStyle::new(13.).mono();
    let widest = status
        .models
        .iter()
        .map(|m| ui.p.layout(&m.name, mono, None).width)
        .fold(0f32, f32::max);
    let sw = (widest + 60.).clamp(160., w);
    if let Some(i) = select(ui, MODEL, Rect::new(x, y, sw, 34.), &items, sel, !busy) {
        d.models.selected = status.models[i].name.clone();
    }
    y += 34. + 12.;
    y += field_label(ui, x, y, w, &d.t("models.prompt"));
    text_field(
        ui,
        &mut d.models.prompt,
        Rect::new(x, y, w, 34.),
        None,
        !busy,
    );
    let enter = !busy && ui.key_for(PROMPT, NamedKey::Enter);
    y += 34. + 12.;
    let label = d.t("models.run");
    let bw = button::width(ui, &label, Some(Icon::Start.id()), false);
    if button::button(
        ui,
        RUN,
        Rect::new(x, y, bw, 32.),
        &label,
        Some(Icon::Start.id()),
        ButtonKind::Primary,
        !busy,
    ) || enter
    {
        run(d);
    }
    y += 32.;
    if !d.models.response.is_empty() {
        y += 14.;
        let style = ui.style().with_size(13.).weight(700);
        y += ui.paragraph(x, y, w, &d.t("models.response"), style, t.ink) + 8.;
        let text = d.models.response.clone();
        let body = ui.style();
        let layout = ui.p.layout_wrapped(&text, body, w - 22.);
        let h = layout.height + 20.;
        ui.p.bordered(Rect::new(x, y, w, h), 7., t.surface, 1., t.line);
        selectable_text(
            ui,
            Id::of("model-response"),
            Point::new(x + 11., y + 10.),
            w - 22.,
            &text,
            body,
            t.ink,
        );
        y += h;
    }
    y - y0
}
