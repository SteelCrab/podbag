//! The run form: a new container from an image, like `docker run -d`. The image (with
//! the local tags to pick from), a name and ports, and under "More options" variables,
//! volumes, the command and the restart policy. When the engine lacks the image, the
//! core pulls it first; its progress shows in the status line. Once the container runs,
//! its logs open.

use podbag_desktop_core::{PublishedPort, RESTART_POLICIES, RunSpec, VolumeMount};
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::dialog::{buttons_row, dialog};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{Color, CursorIcon, Id, Rect, TextInput, TextStyle, icons, layer};

use crate::app::{Desktop, Tab};
use crate::dialogs::{self, Dialog};
use crate::icons::Icon;
use crate::lists::short_id;
use crate::tr::Tr;
use crate::views::blocks::input;

pub const IMAGE: Id = Id(0x0072_756e_0001);
pub const NAME: Id = Id(0x0072_756e_0002);
pub const COMMAND: Id = Id(0x0072_756e_0003);
pub const RESTART: Id = Id(0x0072_756e_0004);
pub const AUTO_REMOVE: Id = Id(0x0072_756e_0005);
pub const MORE: Id = Id(0x0072_756e_0006);
pub const ADD_PORT: Id = Id(0x0072_756e_0007);
pub const ADD_ENV: Id = Id(0x0072_756e_0008);
pub const ADD_VOLUME: Id = Id(0x0072_756e_0009);
/// The arrow that opens the image list.
pub const IMAGES: Id = Id(0x0072_756e_000a);
const BODY: Id = Id(0x0072_756e_000b);
/// The Run button.
pub const RUN: Id = dialogs::YES;
/// The toolbar's and the empty page's Run container.
pub const OPEN: Id = Id(0x0072_756e_000c);
pub const EMPTY_OPEN: Id = Id(0x0072_756e_000d);

/// An item of the image list.
pub fn suggestion_id(i: usize) -> Id {
    IMAGES.with(("item", i))
}

const ROW_H: f32 = 34.;
const GAP: f32 = 6.;

/// A published port: host port, container port and protocol.
pub struct PortRow {
    pub host: TextInput,
    pub container: TextInput,
    pub udp: bool,
    key: u64,
}

impl PortRow {
    pub fn protocol_id(&self) -> Id {
        Id::of(("run-port", self.key, "protocol"))
    }

    pub fn remove_id(&self) -> Id {
        Id::of(("run-port", self.key, "remove"))
    }
}

/// A `KEY=VALUE` variable.
pub struct EnvRow {
    pub input: TextInput,
    key: u64,
}

impl EnvRow {
    pub fn remove_id(&self) -> Id {
        Id::of(("run-env", self.key, "remove"))
    }
}

/// A volume or a host folder, at a path in the container.
pub struct VolumeRow {
    pub source: TextInput,
    pub target: TextInput,
    pub read_only: bool,
    key: u64,
}

impl VolumeRow {
    pub fn read_only_id(&self) -> Id {
        Id::of(("run-volume", self.key, "read-only"))
    }

    pub fn remove_id(&self) -> Id {
        Id::of(("run-volume", self.key, "remove"))
    }
}

/// What is wrong with the form: the field, and the message's key and `port`.
#[derive(Debug, PartialEq, Eq)]
pub struct Problem {
    pub field: Id,
    pub key: &'static str,
    pub port: Option<String>,
}

pub struct RunForm {
    pub image: TextInput,
    pub name: TextInput,
    pub ports: Vec<PortRow>,
    pub env: Vec<EnvRow>,
    pub volumes: Vec<VolumeRow>,
    pub command: TextInput,
    /// An index in `RESTART_POLICIES`.
    pub restart: usize,
    pub auto_remove: bool,
    /// "More options" is open.
    pub more: bool,
    /// The status line: progress, or an error (`error`).
    pub status: String,
    pub error: bool,
    /// The field the error is about, until it is edited.
    pub invalid: Option<Id>,
    /// The container is being made: the form is disabled.
    pub running: bool,
    /// The engine's image tags, for the image list.
    pub tags: Vec<String>,
    /// The image list shows; `filter`: only the tags that match the typed text.
    pub suggest: bool,
    filter: bool,
    highlight: Option<usize>,
    /// Scroll the body to this field on the next frame.
    reveal: Option<Id>,
    scroll: ScrollState,
    body_h: f32,
    next_key: u64,
}

/// The window's part of the run: the container started, until the lists show it.
#[derive(Default)]
pub struct RunState {
    started: Option<Started>,
}

struct Started {
    id: String,
    /// The name to show when the lists do not have it.
    name: String,
    /// List refreshes that did not have it yet.
    misses: u8,
}

/// A command line as words: spaces split words; quotes and a backslash keep them. None
/// when a quote is not closed.
pub fn split_command(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' && matches!(chars.peek(), Some('"' | '\\')) {
                word.get_or_insert_default().extend(chars.next());
            } else {
                word.get_or_insert_default().push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            word.get_or_insert_default();
        } else if c == '\\' && chars.peek().is_some() {
            word.get_or_insert_default().extend(chars.next());
        } else if c.is_whitespace() {
            words.extend(word.take());
        } else {
            word.get_or_insert_default().push(c);
        }
    }
    if quote.is_some() {
        return None;
    }
    words.extend(word);
    Some(words)
}

/// A port: a number from 1 to 65535.
pub fn port_number(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|n| *n >= 1)
}

/// `/…`, `\\…` or `C:\…`.
fn is_host_path(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

/// A letter or digit, then letters, digits, `_`, `.` or `-`.
fn plain_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

fn bad(field: Id, key: &'static str) -> Problem {
    Problem {
        field,
        key,
        port: None,
    }
}

impl RunForm {
    pub fn new(image: &str, tags: Vec<String>, tr: &Tr) -> RunForm {
        let mut image_input = input(IMAGE, "nginx:alpine");
        image_input.set_content(image);
        let mut form = RunForm {
            image: image_input,
            name: input(NAME, &tr.t("run.optional")),
            ports: Vec::new(),
            env: Vec::new(),
            volumes: Vec::new(),
            command: TextInput::new(
                COMMAND,
                &tr.t("run.optional"),
                TextStyle::new(13.).mono().line_height(20.),
                Color::TRANSPARENT,
            ),
            restart: 0,
            auto_remove: false,
            more: false,
            status: String::new(),
            error: false,
            invalid: None,
            running: false,
            tags,
            suggest: false,
            filter: false,
            highlight: None,
            reveal: None,
            scroll: ScrollState::default(),
            body_h: 0.,
            next_key: 0,
        };
        form.add_port();
        form
    }

    /// The field that has the focus when the form opens.
    pub fn first_focus(&self) -> Id {
        if self.image.content().is_empty() {
            IMAGE
        } else {
            NAME
        }
    }

    fn key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    /// A new port row; gives the id of its first field.
    pub fn add_port(&mut self) -> Id {
        let key = self.key();
        let host = input(Id::of(("run-port", key, "host")), "8080");
        let id = host.id;
        self.ports.push(PortRow {
            host,
            container: input(Id::of(("run-port", key, "container")), "80"),
            udp: false,
            key,
        });
        id
    }

    pub fn add_env(&mut self) -> Id {
        let key = self.key();
        let row = EnvRow {
            input: input(Id::of(("run-env", key, "input")), "KEY=VALUE"),
            key,
        };
        let id = row.input.id;
        self.env.push(row);
        id
    }

    pub fn add_volume(&mut self, tr: &Tr) -> Id {
        let key = self.key();
        let source = input(
            Id::of(("run-volume", key, "source")),
            &tr.t("run.volumeSource"),
        );
        let id = source.id;
        self.volumes.push(VolumeRow {
            source,
            target: input(Id::of(("run-volume", key, "target")), "/data"),
            read_only: false,
            key,
        });
        id
    }

    /// The text fields, in the order they show.
    fn inputs(&self) -> Vec<Id> {
        let mut ids = vec![IMAGE, NAME];
        ids.extend(self.ports.iter().flat_map(|r| [r.host.id, r.container.id]));
        ids.extend(self.env.iter().map(|r| r.input.id));
        ids.extend(self.volumes.iter().flat_map(|r| [r.source.id, r.target.id]));
        ids.push(COMMAND);
        ids
    }

    /// A field under "More options".
    fn in_more(&self, id: Id) -> bool {
        id == COMMAND
            || self.env.iter().any(|r| r.input.id == id)
            || self
                .volumes
                .iter()
                .any(|r| r.source.id == id || r.target.id == id)
    }

    /// The form as the engine's `RunSpec`, or the first problem.
    pub fn read(&self) -> Result<RunSpec, Problem> {
        let image = self.image.content().trim();
        if image.is_empty() || image.chars().any(char::is_whitespace) {
            return Err(bad(IMAGE, "run.error.image"));
        }
        let name = self.name.content().trim();
        if !name.is_empty() && (name.len() < 2 || !plain_name(name)) {
            return Err(bad(NAME, "run.error.name"));
        }
        let mut spec = RunSpec {
            image: image.into(),
            name: name.into(),
            restart: "no".into(),
            ..RunSpec::default()
        };
        let mut hosts: Vec<String> = Vec::new();
        for row in &self.ports {
            let (host, container) = (row.host.content().trim(), row.container.content().trim());
            if host.is_empty() && container.is_empty() {
                continue;
            }
            if container.is_empty() {
                return Err(bad(row.container.id, "run.error.containerPort"));
            }
            let Some(container_port) = port_number(container) else {
                return Err(bad(row.container.id, "run.error.port"));
            };
            let host_port = if host.is_empty() {
                0
            } else {
                port_number(host).ok_or_else(|| bad(row.host.id, "run.error.port"))?
            };
            let protocol = if row.udp { "udp" } else { "tcp" };
            if host_port != 0 {
                let key = format!("{host_port}/{protocol}");
                if hosts.contains(&key) {
                    return Err(Problem {
                        field: row.host.id,
                        key: "run.error.portTwice",
                        port: Some(key),
                    });
                }
                hosts.push(key);
            }
            spec.ports.push(PublishedPort {
                host: host_port,
                container: container_port,
                protocol: protocol.into(),
            });
        }
        for row in &self.env {
            let kv = row.input.content().trim();
            if kv.is_empty() {
                continue;
            }
            match kv.split_once('=') {
                Some((key, _)) if !key.is_empty() && !key.chars().any(char::is_whitespace) => {}
                _ => return Err(bad(row.input.id, "run.error.env")),
            }
            spec.env.push(kv.into());
        }
        for row in &self.volumes {
            let (source, target) = (row.source.content().trim(), row.target.content().trim());
            if source.is_empty() && target.is_empty() {
                continue;
            }
            if !is_host_path(source) && !plain_name(source) {
                return Err(bad(row.source.id, "run.error.volumeSource"));
            }
            if !target.starts_with('/') || target == "/" {
                return Err(bad(row.target.id, "run.error.volumeTarget"));
            }
            spec.volumes.push(VolumeMount {
                source: source.into(),
                target: target.into(),
                read_only: row.read_only,
            });
        }
        spec.command = split_command(self.command.content())
            .ok_or_else(|| bad(COMMAND, "run.error.command"))?;
        spec.auto_remove = self.auto_remove;
        if !self.auto_remove {
            spec.restart = RESTART_POLICIES[self.restart.min(RESTART_POLICIES.len() - 1)].into();
        }
        Ok(spec)
    }

    /// Show the problem: the message, the field marked and focused, the options open
    /// when it is there.
    fn show(&mut self, tr: &Tr, problem: Problem) -> Id {
        let vars: Vec<(&str, &str)> = problem.port.iter().map(|p| ("port", p.as_str())).collect();
        self.status = tr.tv(problem.key, &vars);
        self.error = true;
        self.invalid = Some(problem.field);
        if self.in_more(problem.field) {
            self.more = true;
        }
        self.reveal = Some(problem.field);
        problem.field
    }

    /// The local tags the image list offers: all, or those that match the typed text.
    pub fn suggestions(&self) -> Vec<&str> {
        let typed = self.image.content().trim().to_lowercase();
        self.tags
            .iter()
            .map(String::as_str)
            .filter(|t| {
                !self.filter
                    || typed.is_empty()
                    || (t.to_lowercase().contains(&typed) && **t != typed)
            })
            .take(8)
            .collect()
    }
}

/// The local image tags, each once.
fn tags(d: &Desktop) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for t in d.lists.images.iter().flat_map(|i| &i.repo_tags) {
        if t != "<none>:<none>" && !tags.contains(t) {
            tags.push(t.clone());
        }
    }
    tags
}

fn running(d: &Desktop) -> bool {
    matches!(&d.dialog, Some(Dialog::Run(f)) if f.running)
}

/// Open the form, with `image` filled in (from an image's Run).
pub fn open(d: &mut Desktop, image: &str) {
    if running(d) || d.engine.as_ref().is_some_and(|e| !e.reachable) {
        return;
    }
    let form = RunForm::new(image, tags(d), &d.tr);
    d.open_dialog(Dialog::Run(Box::new(form)));
}

/// For previews: the form on Containers, filled in, with its options open and a volume.
pub fn fill(d: &mut Desktop) {
    d.go(crate::app::Page::Containers);
    open(d, "");
    let tr = d.tr;
    let Some(Dialog::Run(f)) = &mut d.dialog else {
        return;
    };
    f.image.set_content("nginx:alpine");
    f.name.set_content("web");
    f.ports[0].host.set_content("8080");
    f.ports[0].container.set_content("80");
    f.more = true;
    f.add_volume(&tr);
    let v = &mut f.volumes[0];
    v.source.set_content("site");
    v.target.set_content("/usr/share/nginx/html");
    v.read_only = true;
    f.restart = 3;
    d.focus_next = Some(IMAGE);
}

/// A line of pull progress, while a missing image is pulled.
pub fn progress(d: &mut Desktop, line: &str) {
    let tr = d.tr;
    if let Some(Dialog::Run(f)) = &mut d.dialog
        && f.running
    {
        let image = f.image.content().trim().to_string();
        f.status = format!("{}\n{line}", tr.tv("pull.pulling", &[("name", &image)]));
    }
}

/// The run ended: on success the form closes and the container's logs open once the
/// lists have it; on an error the form shows it and can be sent again.
pub fn finished(d: &mut Desktop, result: Result<String, String>) {
    let form = match &mut d.dialog {
        Some(Dialog::Run(f)) => Some(f),
        _ => None,
    };
    match result {
        Ok(id) => {
            let name = form
                .map(|f| f.name.content().trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| short_id(&id).to_string());
            if matches!(d.dialog, Some(Dialog::Run(_))) {
                d.dialog = None;
            }
            d.run.started = Some(Started {
                id,
                name,
                misses: 0,
            });
            d.refresh();
        }
        Err(e) => match form {
            Some(f) => {
                f.running = false;
                f.error = true;
                f.status = e;
            }
            None => d.toasts.push(e, true),
        },
    }
}

/// The lists came in: name the container that started and open its logs.
pub fn lists_loaded(d: &mut Desktop) {
    let Some(mut started) = d.run.started.take() else {
        return;
    };
    let found = d
        .lists
        .find(podbag_desktop_core::Kind::Containers, &started.id);
    // A refresh that was under way may not have it yet.
    if found.is_none() && started.misses < 1 {
        started.misses += 1;
        d.run.started = Some(started);
        d.refresh_soon();
        return;
    }
    let name = found.map_or(started.name, |i| {
        crate::lists::container_name(&d.lists.containers[i])
    });
    let text = d.tv("done.start", &[("name", &name)]);
    d.toasts.push(text, false);
    if found.is_some() {
        d.open_detail(
            podbag_desktop_core::Kind::Containers,
            &started.id,
            Tab::Logs,
        );
    }
}

/// Check the form and run it.
fn submit(d: &mut Desktop, ui: &mut Ui, f: &mut RunForm) {
    if f.running {
        return;
    }
    f.invalid = None;
    f.suggest = false;
    match f.read() {
        Err(problem) => {
            let field = f.show(&d.tr, problem);
            ui.focus(Some(field));
        }
        Ok(spec) => {
            f.running = true;
            f.error = false;
            f.status = d.tv("run.starting", &[("name", &spec.image)]);
            d.jobs.run_container(spec);
        }
    }
}

/// What a frame of the body asks for.
#[derive(Default)]
struct Asked {
    submit: bool,
    /// Fields whose text changed.
    edited: Vec<Id>,
    /// Where each field is, from the top of the body.
    spots: Vec<(Id, f32)>,
}

/// Paints the form; true when it closes.
pub fn paint_dialog(d: &mut Desktop, ui: &mut Ui, f: &mut RunForm) -> bool {
    let t = ui.theme;
    let tr = d.tr;
    let mut close = false;
    let mut asked = Asked::default();
    let enabled = !f.running;
    let max_card = ui.bounds().h - 48. - 38.;
    let escape = dialog(ui, dialogs::ID, 600., |ui, r| {
        let mut y = r.y;
        y += crate::views::blocks::heading(ui, r.x, y, r.w, &tr.t("run.title"), 16.) + 12.;
        // The status line and the buttons stay below the body; the body scrolls.
        let status_h = if f.status.is_empty() {
            0.
        } else {
            ui.p.layout_wrapped(&f.status, ui.style().with_size(13.), r.w)
                .height
                + 10.
        };
        let footer_h = status_h + 12. + 32. + 2.;
        let view_h = f
            .body_h
            .min((max_card - (y - r.y) - footer_h).max(120.))
            .max(1.);
        let area = Rect::new(r.x - 14., y, r.w + 28., view_h);
        let mut scroll = std::mem::take(&mut f.scroll);
        let mut used = 0.;
        scroll_view(ui, BODY, area, &mut scroll, |ui, origin| {
            used = body(
                ui,
                &tr,
                f,
                origin.x + 14.,
                origin.y,
                origin.w - 28.,
                &mut asked,
            );
            asked.spots.iter_mut().for_each(|s| s.1 -= origin.y);
            used
        });
        if let Some(id) = f.reveal.take()
            && let Some(&(_, top)) = asked.spots.iter().find(|s| s.0 == id)
        {
            let (top, bottom) = (top - 8., top + ROW_H + 8.);
            if top < scroll.offset {
                scroll.offset = top.max(0.);
            } else if bottom > scroll.offset + view_h {
                scroll.offset = (bottom - view_h).max(0.);
            }
            ui.repaint();
        }
        f.scroll = scroll;
        if (used - f.body_h).abs() > 0.5 {
            f.body_h = used;
            ui.repaint();
        }
        y += view_h;
        // The footer, under a line across the card.
        ui.p.fill(Rect::new(r.x - 22., y, r.w + 44., 1.), t.line);
        y += 12.;
        if !f.status.is_empty() {
            let color = if f.error { t.red } else { t.ink2 };
            y += ui.paragraph(r.x, y, r.w, &f.status, ui.style().with_size(13.), color) + 10.;
        }
        let (cancel, run) = (tr.t("action.cancel"), tr.t("action.run"));
        let start = Some(Icon::Start.id());
        let widths = [
            button::width(ui, &run, start, false),
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
            enabled,
        ) {
            close = true;
        }
        if button::button(ui, RUN, rects[0], &run, start, ButtonKind::Primary, enabled) {
            asked.submit = true;
        }
        y + 34. - r.y
    });
    if let Some(id) = f.invalid
        && asked.edited.contains(&id)
    {
        f.invalid = None;
        f.status.clear();
        f.error = false;
    }
    if asked.submit {
        submit(d, ui, f);
    }
    // Esc closes, but not while the container is being made.
    (escape && !f.running) || close
}

fn label(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let style = ui.style().with_size(13.).weight(600);
    let ink = ui.theme.ink;
    ui.p.label(Rect::new(x, y, w, 18.), text, style, ink);
    24.
}

fn hint(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let ink2 = ui.theme.ink2;
    ui.paragraph(x, y, w, text, ui.style().with_size(12.5), ink2)
}

/// A text field of the form, with a red border while it is the one in error.
fn field(
    ui: &mut Ui,
    input: &mut TextInput,
    rect: Rect,
    invalid: Option<Id>,
    enabled: bool,
    asked: &mut Asked,
) {
    if text_field(ui, input, rect, None, enabled) {
        asked.edited.push(input.id);
    }
    if invalid == Some(input.id) {
        let red = ui.theme.red;
        ui.p.bordered(rect, 7., Color::TRANSPARENT, 1.5, red);
    }
    asked.spots.push((input.id, rect.y));
}

/// A small "+ Add …" button; true when clicked.
fn add_button(ui: &mut Ui, id: Id, x: f32, y: f32, text: &str, enabled: bool) -> bool {
    let plus = Some(Icon::Plus.id());
    let w = button::width(ui, text, plus, true);
    button::button(
        ui,
        id,
        Rect::new(x, y, w, 28.),
        text,
        plus,
        ButtonKind::Normal,
        enabled,
    )
}

fn remove_button(ui: &mut Ui, id: Id, x: f32, y: f32, tr: &Tr, enabled: bool) -> bool {
    let r = Rect::new(x, y + (ROW_H - 30.) / 2., 30., 30.);
    let label = tr.t("run.removeRow");
    button::icon_button(
        ui,
        id,
        r,
        Icon::Close.id(),
        &label,
        IconKind::Plain,
        enabled,
    )
}

fn separator(ui: &mut Ui, x: f32, y: f32, text: &str) -> f32 {
    let t = ui.theme;
    let style = ui.style().with_size(16.);
    let l = ui.p.layout(text, style, None);
    ui.p.draw_text(&l, x, y + (ROW_H - l.height) / 2., t.muted);
    l.width
}

/// The body of the form; returns its height.
fn body(ui: &mut Ui, tr: &Tr, f: &mut RunForm, x: f32, y0: f32, w: f32, asked: &mut Asked) -> f32 {
    let t = ui.theme;
    let enabled = !f.running;
    let invalid = f.invalid;
    let mut y = y0;
    // The image and the name, side by side when there is room.
    let two = w >= 412.;
    let col = if two { (w - 12.) / 2. } else { w };
    label(ui, x, y, col, &tr.t("run.image"));
    let image_r = Rect::new(x, y + 24., col, ROW_H);
    image_field(ui, f, image_r, asked);
    let (nx, ny) = if two {
        (x + col + 12., y)
    } else {
        (x, y + 24. + ROW_H + 10.)
    };
    label(ui, nx, ny, col, &tr.t("run.name"));
    field(
        ui,
        &mut f.name,
        Rect::new(nx, ny + 24., col, ROW_H),
        invalid,
        enabled,
        asked,
    );
    y = ny + 24. + ROW_H + 14.;
    // Ports.
    y += label(ui, x, y, w, &tr.t("run.ports"));
    let protocols = [Item::Option("TCP"), Item::Option("UDP")];
    let mut remove = None;
    for (i, row) in f.ports.iter_mut().enumerate() {
        let fixed = 8. + 84. + 30. + 4. * GAP;
        let pw = ((w - fixed) / 2.).clamp(40., 110.);
        let mut rx = x;
        field(
            ui,
            &mut row.host,
            Rect::new(rx, y, pw, ROW_H),
            invalid,
            enabled,
            asked,
        );
        rx += pw + GAP;
        separator(ui, rx, y, ":");
        rx += 8. + GAP;
        field(
            ui,
            &mut row.container,
            Rect::new(rx, y, pw, ROW_H),
            invalid,
            enabled,
            asked,
        );
        rx += pw + GAP;
        let pid = row.protocol_id();
        if enabled && ui.entered(pid) {
            asked.submit = true;
        }
        if let Some(n) = select(
            ui,
            pid,
            Rect::new(rx, y, 84., ROW_H),
            &protocols,
            row.udp as usize,
            enabled,
        ) {
            row.udp = n == 1;
        }
        rx += 84. + GAP;
        if remove_button(ui, row.remove_id(), rx, y, tr, enabled) {
            remove = Some(i);
        }
        y += ROW_H + GAP;
    }
    if let Some(i) = remove {
        f.ports.remove(i);
        ui.focus(Some(ADD_PORT));
    }
    y += hint(ui, x, y, w, &tr.t("run.portsHint")) + 8.;
    if add_button(ui, ADD_PORT, x, y, &tr.t("run.addPort"), enabled) {
        let id = f.add_port();
        ui.focus(Some(id));
        f.reveal = Some(id);
    }
    y += 28. + 14.;
    // More options.
    ui.p.fill(Rect::new(x, y, w, 1.), t.line);
    y += 9.;
    let more = tr.t("run.more");
    let style = ui.style().with_size(13.).weight(600);
    let lw = ui.p.layout(&more, style, None).width;
    let mr = Rect::new(x, y, 20. + lw + 4., 24.);
    let chevron = if f.more {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    ui.p.icon(chevron, Rect::new(x, y + 5., 14., 14.), t.muted);
    ui.p.label(Rect::new(x + 20., y, lw + 2., 24.), &more, style, t.ink);
    ui.p.hitbox(mr, MORE, CursorIcon::Pointer);
    ui.focusable(MORE, FocusKind::Activate, mr, 6.);
    if ui.clicked(MORE) {
        f.more = !f.more;
    }
    y += 24.;
    if f.more {
        y += 12.;
        y += more_options(ui, tr, f, x, y, w, asked);
    }
    // Enter in a text field sends the form.
    if enabled
        && f.inputs().iter().any(|id| ui.has_focus(*id))
        && ui.key_pressed(NamedKey::Enter).is_some()
    {
        asked.submit = true;
    }
    y - y0 + 4.
}

/// Variables, volumes, the command and the restart policy; returns their height.
fn more_options(
    ui: &mut Ui,
    tr: &Tr,
    f: &mut RunForm,
    x: f32,
    y0: f32,
    w: f32,
    asked: &mut Asked,
) -> f32 {
    let enabled = !f.running;
    let invalid = f.invalid;
    let mut y = y0;
    // Variables.
    y += label(ui, x, y, w, &tr.t("run.env"));
    let mut remove = None;
    for (i, row) in f.env.iter_mut().enumerate() {
        field(
            ui,
            &mut row.input,
            Rect::new(x, y, w - 36., ROW_H),
            invalid,
            enabled,
            asked,
        );
        if remove_button(ui, row.remove_id(), x + w - 30., y, tr, enabled) {
            remove = Some(i);
        }
        y += ROW_H + GAP;
    }
    if let Some(i) = remove {
        f.env.remove(i);
        ui.focus(Some(ADD_ENV));
    }
    if add_button(ui, ADD_ENV, x, y, &tr.t("run.addEnv"), enabled) {
        let id = f.add_env();
        ui.focus(Some(id));
        f.reveal = Some(id);
    }
    y += 28. + 14.;
    // Volumes.
    y += label(ui, x, y, w, &tr.t("run.volumes"));
    let ro = tr.t("run.readOnly");
    let ro_w = checkbox::width(ui, &ro);
    let mut remove = None;
    for (i, row) in f.volumes.iter_mut().enumerate() {
        let fixed = 14. + ro_w + 30. + 4. * GAP;
        let vw = ((w - fixed) / 2.).max(40.);
        let mut rx = x;
        field(
            ui,
            &mut row.source,
            Rect::new(rx, y, vw, ROW_H),
            invalid,
            enabled,
            asked,
        );
        rx += vw + GAP;
        separator(ui, rx, y, "→");
        rx += 14. + GAP;
        field(
            ui,
            &mut row.target,
            Rect::new(rx, y, vw, ROW_H),
            invalid,
            enabled,
            asked,
        );
        rx += vw + GAP;
        let cid = row.read_only_id();
        if enabled && ui.entered(cid) {
            asked.submit = true;
        }
        checkbox::checkbox(
            ui,
            cid,
            Rect::new(rx, y, ro_w, ROW_H),
            Some(&mut row.read_only),
            &ro,
            enabled,
        );
        rx += ro_w + GAP;
        if remove_button(ui, row.remove_id(), rx, y, tr, enabled) {
            remove = Some(i);
        }
        y += ROW_H + GAP;
    }
    if let Some(i) = remove {
        f.volumes.remove(i);
        ui.focus(Some(ADD_VOLUME));
    }
    if add_button(ui, ADD_VOLUME, x, y, &tr.t("run.addVolume"), enabled) {
        let id = f.add_volume(tr);
        ui.focus(Some(id));
        f.reveal = Some(id);
    }
    y += 28. + 14.;
    // The command.
    y += label(ui, x, y, w, &tr.t("run.command"));
    field(
        ui,
        &mut f.command,
        Rect::new(x, y, w, ROW_H),
        invalid,
        enabled,
        asked,
    );
    y += ROW_H + 6.;
    y += hint(ui, x, y, w, &tr.t("run.commandHint")) + 12.;
    // The restart policy, and removal when it stops (which goes only with no restart).
    let two = w >= 412.;
    let col = if two { (w - 12.) / 2. } else { w };
    y += label(ui, x, y, col, &tr.t("run.restart"));
    let labels: Vec<String> = RESTART_POLICIES
        .iter()
        .map(|p| tr.t(&format!("run.restart.{p}")))
        .collect();
    let items: Vec<Item> = labels.iter().map(|l| Item::Option(l)).collect();
    if enabled && ui.entered(RESTART) {
        asked.submit = true;
    }
    if let Some(n) = select(
        ui,
        RESTART,
        Rect::new(x, y, col, ROW_H),
        &items,
        f.restart,
        enabled && !f.auto_remove,
    ) {
        f.restart = n;
    }
    let (cx, cy) = if two {
        (x + col + 12., y)
    } else {
        (x, y + ROW_H + 8.)
    };
    if enabled && ui.entered(AUTO_REMOVE) {
        asked.submit = true;
    }
    let rm = tr.t("run.autoRemove");
    if checkbox::checkbox(
        ui,
        AUTO_REMOVE,
        Rect::new(cx, cy, col, ROW_H),
        Some(&mut f.auto_remove),
        &rm,
        enabled,
    ) && f.auto_remove
    {
        f.restart = 0;
    }
    cy + ROW_H - y0
}

/// The image field, with the local tags to pick from below it (typing filters them; the
/// arrow at its right, or the Down key, shows them all).
fn image_field(ui: &mut Ui, f: &mut RunForm, r: Rect, asked: &mut Asked) {
    let t = ui.theme;
    let enabled = !f.running;
    let shown = f.suggestions().len();
    let open = f.suggest && shown > 0;
    // Keys for the list go before the field's own.
    if enabled && ui.key_for(IMAGE, NamedKey::ArrowDown) {
        if open {
            f.highlight = Some(f.highlight.map_or(0, |h| (h + 1).min(shown - 1)));
        } else {
            f.suggest = true;
            f.filter = false;
            f.highlight = Some(0);
        }
    } else if open && ui.key_for(IMAGE, NamedKey::ArrowUp) {
        f.highlight = f.highlight.map(|h| h.saturating_sub(1));
    } else if open && ui.key_for(IMAGE, NamedKey::Escape) {
        f.suggest = false;
    } else if open
        && let Some(h) = f.highlight
        && ui.key_for(IMAGE, NamedKey::Enter)
    {
        pick(f, h, asked);
    }
    let before = f.image.content().to_string();
    field(ui, &mut f.image, r, f.invalid, enabled, asked);
    if f.image.content() != before {
        f.suggest = true;
        f.filter = true;
        f.highlight = None;
    }
    if f.tags.is_empty() || !enabled {
        return;
    }
    // The arrow: clicking it keeps the focus in the field.
    let arrow = Rect::new(r.right() - 30., r.y + 2., 28., r.h - 4.);
    ui.p.icon(icons::CHEVRON_DOWN, arrow.center(14., 14.), t.ink2);
    ui.p.hitbox(arrow, IMAGES, CursorIcon::Pointer);
    ui.popup_hitbox(IMAGES);
    if ui.clicked(IMAGES) {
        // The arrow shows every tag, or hides them when they show.
        f.suggest = !f.suggest || f.filter;
        f.filter = false;
        f.highlight = None;
        ui.focus(Some(IMAGE));
    }
    if !ui.has_focus(IMAGE) {
        f.suggest = false;
    }
    let list: Vec<String> = f.suggestions().into_iter().map(str::to_string).collect();
    if !f.suggest || list.is_empty() {
        return;
    }
    let menu = Rect::new(r.x, r.bottom() + 4., r.w, list.len() as f32 * 28. + 8.);
    let style = ui.style();
    let mut chosen = None;
    ui.on_layer(layer::MODAL_POPUP, |ui| {
        ui.p.shadow(menu, 9., 14., t.shadow);
        ui.p.bordered(menu, 9., t.surface, 1., t.line);
        for (i, tag) in list.iter().enumerate() {
            let ir = Rect::new(menu.x + 4., menu.y + 4. + i as f32 * 28., menu.w - 8., 28.);
            let id = suggestion_id(i);
            let lit = f.highlight == Some(i) || ui.hovered(id);
            if lit {
                ui.p.rounded(ir, 6., t.accent_soft);
            }
            let fg = if lit { t.accent_text } else { t.ink };
            ui.p.label(ir.inset(10., 0., 8., 0.), tag, style, fg);
            ui.p.hitbox(ir, id, CursorIcon::Pointer);
            ui.popup_hitbox(id);
            if ui.clicked(id) {
                chosen = Some(i);
            }
        }
    });
    if let Some(i) = chosen {
        pick(f, i, asked);
        ui.focus(Some(IMAGE));
    }
}

fn pick(f: &mut RunForm, i: usize, asked: &mut Asked) {
    if let Some(tag) = f.suggestions().get(i).map(|s| s.to_string()) {
        f.image.set_content(&tag);
        asked.edited.push(IMAGE);
    }
    f.suggest = false;
    f.highlight = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(
            split_command("nginx -g 'daemon off;'"),
            Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()])
        );
        assert_eq!(split_command("nginx -g 'daemon off;"), None);
        assert_eq!(
            split_command(r#"echo "a \"b\" \\ c" d\ e ''"#),
            Some(vec![
                "echo".into(),
                r#"a "b" \ c"#.into(),
                "d e".into(),
                "".into()
            ])
        );
        assert_eq!(split_command("  "), Some(vec![]));
    }

    #[test]
    fn ports_and_paths() {
        assert_eq!(port_number("80"), Some(80));
        assert_eq!(port_number("65535"), Some(65535));
        for bad in ["", "0", "70000", "8o", "-1", " 80"] {
            assert_eq!(port_number(bad), None, "{bad}");
        }
        assert!(
            is_host_path("/Users/me") && is_host_path("C:\\site") && is_host_path("\\\\host\\x")
        );
        assert!(!is_host_path("./site") && !is_host_path("site"));
        assert!(plain_name("site") && !plain_name("./site") && !plain_name("-x"));
    }
}
