//! The pages, and the parts several pages share: the toolbar, chips, empty states,
//! commands to copy and the label/value grids.

pub mod blocks;
pub mod compression;
pub mod detail;
pub mod import;
pub mod kubernetes;
pub mod login;
pub mod migrate;
pub mod models;
pub mod network_settings;
pub mod notices;
pub mod practice;
pub mod pull;
pub mod resources;
pub mod run;
pub mod settings;
pub mod settings_search;
pub mod sync_shares;
pub mod training;
pub mod updates;

use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::selectable::selectable_text;
use podbag_ui::{Color, Id, Point, Rect, TextStyle};

use crate::app::{Desktop, Page};
use crate::icons::Icon;

/// The main area, right of the sidebar.
pub fn paint_main(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    match d.page {
        Page::Kubernetes => kubernetes::paint(d, ui, r),
        Page::Settings => settings::paint(d, ui, r),
        Page::Training => training::paint(d, ui, r),
        Page::LabContainers | Page::LabVolumes => practice::paint(d, ui, r),
        Page::Migrate => migrate::paint(d, ui, r),
        Page::Models => models::paint(d, ui, r),
        page => {
            let Some(kind) = page.kind() else { return };
            if d.engine.as_ref().is_some_and(|e| !e.reachable) {
                let mut body = r;
                let bar = body.cut_top(TOOLBAR_H);
                title_bar(ui, bar, &d.t(&format!("nav.{}", kind.segment())));
                offline(d, ui, body);
            } else if d.detail.is_some() {
                detail::paint(d, ui, r);
            } else {
                resources::paint(d, ui, r, kind);
            }
        }
    }
}

pub const TOOLBAR_H: f32 = 62.;

/// A page was (re)entered: Training, Migrate, the practice pages, Models and Settings
/// read what they show.
pub fn entered(d: &mut Desktop) {
    match d.page {
        Page::Training => training::entered(d),
        Page::LabContainers | Page::LabVolumes => practice::entered(d),
        Page::Migrate => migrate::reload(d),
        Page::Models => models::entered(d),
        Page::Settings => compression::read(d),
        _ => {}
    }
}

/// The pages' timers (the practice polls); true if one ran.
pub fn tick(d: &mut Desktop, now: std::time::Instant) -> bool {
    training::tick(d, now) | practice::tick(d, now)
}

pub fn next_due(d: &Desktop) -> Option<std::time::Instant> {
    [training::next_due(d), practice::next_due(d)]
        .into_iter()
        .flatten()
        .min()
}

/// The pages' preview commands: `training [<id>]`, `training install <id>`,
/// `training refresh` (a sync),
/// `lab start|advance <id>` (the mock's terminal), `labs <action>` (the JSON lab),
/// `migrate engine|kube [kind]`, `migration <n>` (its dialog), `check updates`,
/// `upgrade`, `login <location>`, `import`, `run [image]` (the run form), `run-fill` (the
/// form filled in, with its options open), `notice cli|update`, `star`, `models
/// start|stop|status`, `models pull <name>`, `models run <prompt>`, `search <text>` (in
/// Settings), `sync add <name> <path>`, `migrate-source <engine>`, `registry-host <location>
/// <host>` (a registry host the user gave).
pub fn command(d: &mut Desktop, line: &str) {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["training"] => training::show(d, None),
        ["training", "install", id] => {
            if let Some(text) = podbag_desktop_core::mock::sample_module(id) {
                training::install(d, Some(id.to_string()), text);
            }
        }
        ["training", "refresh"] => training::sync(d, true),
        ["training", "lesson"] => {
            let pack = serde_json::json!({
                "format": "podbag-volume-api-lab-v1",
                "starter": "{\"message\":\"hello\"}",
                "translations": {
                    "en": { "title": "Lab 1 · My HTTP API", "goal": "Serve a JSON file from a volume and read it over HTTP.",
                        "create": "Name a volume for the lab.", "write": "Write the JSON the server answers with.",
                        "serve": "Start a local server for the file.", "request": "Send a request to the server." },
                    "ko": { "title": "실습 1 · 나의 HTTP API", "goal": "볼륨의 JSON 파일을 HTTP로 제공하고 읽습니다.",
                        "create": "실습용 볼륨 이름을 정하세요.", "write": "서버가 응답할 JSON을 작성하세요.",
                        "serve": "파일을 제공할 로컬 서버를 시작하세요.", "request": "서버에 요청을 보내세요." },
                },
            });
            training::install(d, None, pack.to_string());
        }
        ["training", id] => training::show(d, Some(id.to_string())),
        ["lab", "start", id] => {
            let _ = d.api.practice("start", None, Some(id), false, false);
            training::poll(d);
            practice::poll(d);
        }
        ["lab", "advance"] => {
            // Only the flavor of the running session moves on.
            for (containers, course) in [(false, false), (true, false), (false, true)] {
                let _ = d.api.practice("advance", None, None, containers, course);
            }
            training::poll(d);
            practice::poll(d);
        }
        ["labs", action, rest @ ..] => {
            let action: &'static str = match *action {
                "start" => "start",
                "create" => "create",
                "save" => "save",
                "serve" => "serve",
                "request" => "request",
                _ => "end",
            };
            let value = (!rest.is_empty()).then(|| rest.join(" "));
            training::lab(d, action, value);
        }
        ["migrate", group, rest @ ..] => {
            migrate::open(d, *group == "kube", rest.first().copied().unwrap_or(""));
        }
        ["migrate-source", engine] => {
            d.migrate.engine = engine.to_string();
            migrate::reload(d);
        }
        ["registry-host", id, host] => {
            d.pull_hosts.retain(|(i, _)| i != id);
            d.pull_hosts.push((id.to_string(), host.to_string()));
        }
        ["migration", "start"] => migrate::start_shown(d),
        ["migration", n] => migrate::confirm_nth(d, n.parse().unwrap_or(0)),
        ["check", "updates"] => updates::load(d, podbag_desktop_core::updates::Check::Now),
        ["upgrade"] => updates::run_upgrade(d),
        ["import"] => import::open(d),
        ["run"] => run::open(d, ""),
        ["run", image] => run::open(d, image),
        ["run-fill"] => run::fill(d),
        ["login", location] => {
            d.open_pull();
            if let Some(p) = &mut d.pull {
                login::start(p, location);
            }
        }
        ["notice", "cli"] => {
            d.notices.cli_notice = Some(notices::Notice::default());
            if d.notices.cli.is_none() {
                d.notices.cli = d.api.terminal_command();
            }
        }
        ["star"] => d.open_dialog(crate::dialogs::Dialog::Star),
        ["models", action, rest @ ..] => {
            use podbag_desktop_core::model_runner::Action;
            let text = rest.join(" ");
            let action = match *action {
                "start" => Action::Start,
                "stop" => Action::Stop,
                "pull" => Action::Pull { model: text },
                "run" => {
                    d.models.prompt.set_content(&text);
                    let model = d
                        .models
                        .status
                        .as_ref()
                        .and_then(|s| s.models.first())
                        .map_or(String::new(), |m| m.name.clone());
                    d.models.selected = model.clone();
                    Action::Run {
                        model,
                        prompt: text,
                    }
                }
                _ => Action::Status,
            };
            if d.page != crate::app::Page::Models {
                d.go(crate::app::Page::Models);
                d.pump();
            }
            models::act(d, action);
        }
        ["search", rest @ ..] => {
            if d.page != crate::app::Page::Settings {
                d.go(crate::app::Page::Settings);
            }
            d.settings_search.input.set_content(&rest.join(" "));
            d.focus_next = Some(settings_search::SEARCH);
        }
        ["sync", "add", name, path @ ..] => sync_shares::act(
            d,
            podbag_desktop_core::vm::SyncAction::Add {
                name: name.to_string(),
                path: path.join(" "),
            },
        ),
        _ => {}
    }
}

/// The page header's frame and title; returns the title's right edge.
pub fn title_bar(ui: &mut Ui, bar: Rect, title: &str) -> f32 {
    let t = ui.theme;
    ui.p.border_bottom(bar, t.line);
    let style = ui.style().with_size(18.).weight(700);
    let layout = ui.p.layout(title, style, Some((bar.w * 0.4).max(80.)));
    ui.p.draw_text(
        &layout,
        bar.x + 20.,
        bar.y + (bar.h - 1. - layout.height) / 2.,
        t.ink,
    );
    bar.x + 20. + layout.width
}

/// The square icon above an empty state.
pub fn badge(ui: &mut Ui, cx: f32, y: f32, icon: Icon) {
    let t = ui.theme;
    ui.p.rounded(Rect::new(cx - 32., y, 64., 64.), 16., t.accent_soft);
    ui.p.icon(
        icon.id(),
        Rect::new(cx - 16., y + 16., 32., 32.),
        t.accent_text,
    );
}

/// Wrapped text centered on `cx`; returns its height.
pub fn centered(
    ui: &mut Ui,
    cx: f32,
    y: f32,
    max_w: f32,
    text: &str,
    style: TextStyle,
    color: Color,
) -> f32 {
    let layout = ui.p.layout_wrapped(text, style, max_w.clamp(40., 520.));
    ui.p.draw_text(&layout, cx - layout.width / 2., y, color);
    layout.height
}

/// A pill with a dot. Returns its width.
pub fn chip(d: &Desktop, ui: &mut Ui, x: f32, cy: f32, state: &str, max_w: f32) -> f32 {
    let t = ui.theme;
    let known = [
        "running",
        "exited",
        "created",
        "paused",
        "restarting",
        "dead",
    ];
    let s = if known.contains(&state) {
        state
    } else {
        "created"
    };
    let (fg, bg) = match s {
        "running" => (t.green, t.green_soft),
        "paused" | "restarting" => (t.amber, t.amber_soft),
        "dead" => (t.red, t.red_soft),
        _ => (t.ink2, t.gray_soft),
    };
    let label = d.t(&format!("state.{s}"));
    pill(ui, x, cy, &label, fg, bg, max_w)
}

/// A rounded label with a dot, vertically centered on `cy`. Returns its width.
pub fn pill(ui: &mut Ui, x: f32, cy: f32, label: &str, fg: Color, bg: Color, max_w: f32) -> f32 {
    let style = ui.style().with_size(12.5).weight(600).line_height(18.);
    let text = ui.p.layout(label, style, Some((max_w - 29.).max(10.)));
    let w = text.width + 29.;
    let r = Rect::new(x, cy - 11., w, 22.);
    ui.p.rounded(r, 11., bg);
    ui.p.rounded(Rect::new(r.x + 8., cy - 3.5, 7., 7.), 3.5, fg);
    ui.p.draw_text(&text, r.x + 20., cy - 9., fg);
    w
}

/// A command in monospace with a copy button, centered on `cx`; returns its height.
pub fn code_line(d: &mut Desktop, ui: &mut Ui, cx: f32, y: f32, text: &str, id: Id) -> f32 {
    let t = ui.theme;
    let style = TextStyle::new(13.).mono().line_height(18.);
    let layout = ui.p.layout(text, style, None);
    let w = layout.width + 14. + 6. + 30. + 6.;
    let r = Rect::new(cx - w / 2., y, w, 42.);
    ui.p.bordered(r, 8., t.sunken, 1., t.line);
    ui.p.draw_text(&layout, r.x + 14., r.y + 12., t.ink);
    let label = d.t("action.copy");
    let b = Rect::new(r.right() - 36., r.y + 6., 30., 30.);
    if button::icon_button(ui, id, b, Icon::Copy.id(), &label, IconKind::Plain, true) {
        d.copy(text, ui);
    }
    r.h
}

/// A centered button; returns true when clicked.
fn center_button(
    ui: &mut Ui,
    id: Id,
    cx: f32,
    y: f32,
    label: &str,
    icon: Option<Icon>,
    kind: ButtonKind,
) -> bool {
    let icon = icon.map(Icon::id);
    let w = button::width(ui, label, icon, false);
    button::button(
        ui,
        id,
        Rect::new(cx - w / 2., y, w, 32.),
        label,
        icon,
        kind,
        true,
    )
}

/// Why the engine does not answer, and what to do.
pub fn offline(d: &mut Desktop, ui: &mut Ui, r: Rect) {
    let t = ui.theme;
    let Some(e) = d.engine.clone() else { return };
    let cx = r.x + r.w / 2.;
    let max_w = r.w - 48.;
    let mut y = r.y + 56.;
    let h2 = ui.style().with_size(18.).weight(700);
    let body = ui.style();
    let mono = TextStyle::new(12.5).mono().line_height(18.);
    if e.engine == "docker" {
        badge(ui, cx, y, Icon::Engine);
        y += 80.;
        y += centered(ui, cx, y, max_w, &d.t("offline.dockerTitle"), h2, t.ink) + 8.;
        if e.platform == "linux" {
            y += centered(ui, cx, y, max_w, &d.t("offline.dockerLinux"), body, t.ink2) + 8.;
            y += code_line(
                d,
                ui,
                cx,
                y,
                "sudo systemctl start docker",
                Id::of("offline-code"),
            ) + 14.;
        } else {
            y += centered(ui, cx, y, max_w, &d.t("offline.dockerText"), body, t.ink2) + 14.;
        }
        let retry = d.t("action.retry");
        let other = d.tv("switch.to", &[("name", "podbag")]);
        let (rw, ow) = (
            button::width(ui, &retry, Some(Icon::Refresh.id()), false),
            button::width(ui, &other, None, false),
        );
        let x = cx - (rw + 8. + ow) / 2.;
        if button::button(
            ui,
            Id::of("offline-retry"),
            Rect::new(x, y, rw, 32.),
            &retry,
            Some(Icon::Refresh.id()),
            ButtonKind::Primary,
            true,
        ) {
            d.connect(false);
        }
        if button::button(
            ui,
            Id::of("offline-podbag"),
            Rect::new(x + rw + 8., y, ow, 32.),
            &other,
            None,
            ButtonKind::Normal,
            true,
        ) && let Err(err) = d.switch_engine("podbag")
        {
            d.toasts.push(err, true);
        }
        return;
    }
    if !d.connection.local {
        y += centered(ui, cx, y, max_w, &d.t("engine.down"), h2, t.ink) + 8.;
        let detail = e.error.clone().unwrap_or(e.endpoint.clone());
        y += centered(ui, cx, y, max_w, &detail, mono, t.red) + 14.;
        let label = d.t("action.retry");
        if center_button(
            ui,
            Id::of("offline-retry"),
            cx,
            y,
            &label,
            Some(Icon::Refresh),
            ButtonKind::Primary,
        ) {
            d.connect(false);
        }
        return;
    }
    let vm_off =
        e.error.as_deref() == Some("vm-stopped") || e.vm.as_deref().is_some_and(|v| v != "running");
    // The logo, 72 x 72.
    crate::images::draw_logo(ui, Rect::new(cx - 36., y - 4., 72., 72.));
    y += 80.;
    if vm_off {
        y += centered(ui, cx, y, max_w, &d.t("offline.vmTitle"), h2, t.ink) + 8.;
        y += centered(ui, cx, y, max_w, &d.t("offline.vmText"), body, t.ink2) + 14.;
        let label = d.t("action.startVm");
        if center_button(
            ui,
            Id::of("offline-start"),
            cx,
            y,
            &label,
            Some(Icon::Start),
            ButtonKind::Primary,
        ) {
            d.connect(true);
        }
        return;
    }
    y += centered(ui, cx, y, max_w, &d.t("offline.title"), h2, t.ink) + 8.;
    y += centered(ui, cx, y, max_w, &d.t("offline.text"), body, t.ink2) + 8.;
    if let Some(err) = e.error.as_deref().filter(|e| *e != "vm-stopped") {
        y += centered(ui, cx, y, max_w, err, mono, t.red) + 8.;
    }
    y += centered(ui, cx, y, max_w, &d.t("offline.hint"), body, t.ink2) + 8.;
    let hint = if e.platform == "linux" {
        "podbag system service"
    } else {
        "podbag vm start"
    };
    y += code_line(d, ui, cx, y, hint, Id::of("offline-code")) + 14.;
    let label = d.t("action.retry");
    if center_button(
        ui,
        Id::of("offline-retry"),
        cx,
        y,
        &label,
        Some(Icon::Refresh),
        ButtonKind::Primary,
    ) {
        d.connect(false);
    }
}

/// One row of a label/value grid: the label, the value and whether it is monospace,
/// and a muted line below the value (empty: none).
pub struct Field {
    pub label: String,
    pub value: String,
    pub mono: bool,
    pub note: String,
}

pub fn field(label: String, value: impl Into<String>, mono: bool) -> Field {
    Field {
        label,
        value: value.into(),
        mono,
        note: String::new(),
    }
}

/// A label/value grid: labels left, selectable values right. Returns the height used.
pub fn fields(ui: &mut Ui, id: Id, x: f32, y: f32, w: f32, rows: &[Field], none: &str) -> f32 {
    let t = ui.theme;
    let style = ui.style();
    let mono = TextStyle::new(12.5).mono().line_height(20.);
    let label_w = rows
        .iter()
        .map(|f| ui.p.layout(&f.label, style, None).width)
        .fold(110f32, f32::max)
        .min(w * 0.4);
    let mut yy = y;
    for (i, f) in rows.iter().enumerate() {
        ui.label(
            Rect::new(x, yy, label_w, style.line_height),
            &f.label,
            style,
            t.ink2,
        );
        let (text, s) = if f.value.is_empty() {
            (none, style)
        } else {
            (f.value.as_str(), if f.mono { mono } else { style })
        };
        let vx = x + label_w + 22.;
        let h = selectable_text(
            ui,
            id.with(i),
            Point::new(vx, yy),
            (w - label_w - 22.).max(40.),
            text,
            s,
            t.ink,
        );
        yy += h.max(style.line_height);
        if !f.note.is_empty() {
            let w = (w - label_w - 22.).max(40.);
            yy += 2. + ui.paragraph(vx, yy + 2., w, &f.note, style.with_size(13.), t.ink2);
        }
        yy += 10.;
    }
    yy - y
}

pub fn vm_state_key(state: &str) -> &'static str {
    match state {
        "running" => "running",
        "standby" => "standby",
        "starting" => "starting",
        "stopped" => "stopped",
        "not set up" => "notSetUp",
        _ => "unknown",
    }
}
