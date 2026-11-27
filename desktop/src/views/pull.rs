//! The pull panel: search Docker Hub or type a reference, pick a tag,
//! choose where to pull from (with the registry's address where it needs one), then
//! watch the progress stream. A location's "Log in" asks for a user and a password
//! (`views::login`).

use std::time::{Duration, Instant};

use podbag_desktop_core::SearchResult;
use podbag_desktop_core::images::{LocationInfo, Search};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind as ButtonKind};
use podbag_ui::widgets::text_field::{plain_field, text_field};
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{Color, CursorIcon, Id, Rect, TextInput, TextStyle, icons, layer};

use crate::app::Desktop;
use crate::icons::Icon;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Search,
    Tag,
    Host,
    Login,
    Pulling,
    Done,
}

pub const INPUT: Id = Id(0x0070_756c_6c01);
pub const TAG: Id = Id(0x0070_756c_6c02);
pub const HOST: Id = Id(0x0070_756c_6c03);
pub const LOCATION: Id = Id(0x0070_756c_6c04);
pub const MENU_SEARCH: Id = Id(0x0070_756c_6c05);
pub const TAG_PULL: Id = Id(0x0070_756c_6c06);
pub const HOST_SAVE: Id = Id(0x0070_756c_6c07);
pub const CLOSE: Id = Id(0x0070_756c_6c08);
pub const ANOTHER: Id = Id(0x0070_756c_6c09);
pub const BACKDROP: Id = Id(0x0070_756c_6c0a);

/// One row of the search step: "Pull <typed>", or a Docker Hub result.
#[derive(Clone)]
struct Choice {
    typed: bool,
    result: SearchResult,
}

pub struct PullPanel {
    pub input: TextInput,
    pub location: String,
    pub locations: Vec<LocationInfo>,
    pub results: Vec<SearchResult>,
    pub active: usize,
    pub step: Step,
    chosen: Option<SearchResult>,
    pub supported: bool,
    pub search_error: Option<String>,
    pub lines: Vec<String>,
    pub pulling: String,
    pub result: Option<Result<String, String>>,
    pub tag: TextInput,
    pub host: TextInput,
    pub menu_open: bool,
    pub menu_search: TextInput,
    pub search_at: Option<Instant>,
    /// Give the focus to this widget on the next frame.
    pub(crate) focus: Option<Id>,
    /// The step after the host is given: the search or the login.
    pub after_host: Option<Step>,
    pub login: crate::views::login::Login,
}

fn input(id: Id, size: f32) -> TextInput {
    TextInput::new(
        id,
        "",
        TextStyle::new(size).line_height((size * 1.45).round()),
        Color::TRANSPARENT,
    )
}

/// A reference the user pasted whole: with a tag, a digest or a registry host.
pub fn looks_full(q: &str) -> bool {
    if q.contains(':') || q.contains('@') || q.starts_with("localhost/") {
        return true;
    }
    q.split_once('/')
        .is_some_and(|(first, _)| first.contains('.') || first.contains(':'))
}

impl PullPanel {
    pub fn new(locations: Vec<LocationInfo>, placeholder: &str) -> Self {
        let mut i = input(INPUT, 15.);
        i.set_placeholder(placeholder);
        Self {
            input: i,
            location: "dockerhub".into(),
            locations,
            results: Vec::new(),
            active: 0,
            step: Step::Search,
            chosen: None,
            supported: true,
            search_error: None,
            lines: Vec::new(),
            pulling: String::new(),
            result: None,
            tag: input(TAG, 14.),
            host: input(HOST, 14.),
            menu_open: false,
            menu_search: input(MENU_SEARCH, 13.),
            search_at: None,
            focus: Some(INPUT),
            after_host: None,
            login: crate::views::login::Login::default(),
        }
    }

    /// The location chosen.
    pub fn current(&self) -> LocationInfo {
        self.location()
    }

    fn location(&self) -> LocationInfo {
        self.locations
            .iter()
            .find(|l| l.id == self.location)
            .cloned()
            .unwrap_or(LocationInfo {
                id: "dockerhub",
                label: "Docker Hub".into(),
                host: "docker.io".into(),
                host_example: "",
                needs_host: false,
                account: None,
                hint: None,
            })
    }

    pub fn progress(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > 200 {
            self.lines.drain(..self.lines.len() - 200);
        }
    }

    pub fn done(&mut self, result: Result<String, String>) {
        self.step = Step::Done;
        self.result = Some(result);
        self.focus = Some(CLOSE);
    }

    /// The typed text changed: search Docker Hub after a short pause.
    pub fn on_input(&mut self, now: bool) {
        self.active = 0;
        self.step = Step::Search;
        let q = self.input.content().trim().to_string();
        if q.is_empty() || looks_full(&q) || self.location != "dockerhub" || !self.supported {
            self.results.clear();
            self.search_at = None;
            return;
        }
        let wait = if now { 0 } else { 300 };
        self.search_at = Some(Instant::now() + Duration::from_millis(wait));
    }

    /// The search term, when the pause after typing is over.
    pub fn due_search(&mut self, now: Instant) -> Option<String> {
        if self.search_at.is_some_and(|at| at <= now) {
            self.search_at = None;
            return Some(self.input.content().trim().to_string());
        }
        None
    }

    /// A search ended; it counts if the text is still the same.
    pub fn searched(&mut self, term: &str, r: Search) {
        if self.location != "dockerhub" || self.input.content().trim() != term {
            return;
        }
        self.supported = r.supported;
        self.search_error = r.error;
        self.results = r.results;
    }

    /// "Pull <typed>", then the search results.
    fn options(&self) -> Vec<Choice> {
        let q = self.input.content().trim();
        let mut out = Vec::new();
        if !q.is_empty() {
            let mut typed = self
                .results
                .iter()
                .find(|r| r.name == q)
                .cloned()
                .unwrap_or_default();
            typed.name = q.to_string();
            out.push(Choice {
                typed: true,
                result: typed,
            });
        }
        out.extend(
            self.results
                .iter()
                .filter(|r| r.name != q)
                .cloned()
                .map(|result| Choice {
                    typed: false,
                    result,
                }),
        );
        out
    }
}

fn start_pull(d: &Desktop, p: &mut PullPanel, image: String) {
    let loc = p.location();
    if loc.needs_host && loc.host.is_empty() {
        p.host.set_content("");
        p.step = Step::Host;
        p.focus = Some(HOST);
        return;
    }
    p.step = Step::Pulling;
    p.lines = vec![d.t("pull.working")];
    p.pulling = image.clone();
    p.result = None;
    let host = (!loc.host.is_empty()).then_some(loc.host.as_str());
    d.jobs.pull(&image, Some(loc.id), host);
}

pub fn paint(d: &mut Desktop, ui: &mut Ui) {
    let t = ui.theme;
    let bounds = ui.bounds();
    let Some(mut p) = d.pull.take() else { return };
    if let Some(id) = p.focus.take() {
        ui.focus(Some(id));
    }
    let busy = p.step == Step::Pulling;
    let mut close = false;
    // Escape steps back first (the menu, the tag or host step), then closes.
    if ui.key_pressed(NamedKey::Escape).is_some() {
        if p.menu_open {
            p.menu_open = false;
            p.focus = Some(LOCATION);
        } else if matches!(p.step, Step::Tag | Step::Host | Step::Login) && !p.login.busy {
            p.step = Step::Search;
            p.focus = Some(INPUT);
        } else {
            close = true;
        }
    }
    let w = (bounds.w - 32.).min(560.);
    let panel_h = (bounds.h - 64. - 72.).clamp(160., 560.);
    let card = Rect::new(
        (bounds.right() - 24. - w).max(bounds.x + 16.),
        bounds.y + 72.,
        w,
        panel_h,
    );
    ui.on_layer(layer::MODAL, |ui| {
        ui.p.fill(bounds, t.backdrop);
        ui.p.hitbox(bounds, BACKDROP, CursorIcon::Default);
        if ui.clicked(BACKDROP) {
            close = true;
        }
        ui.p.shadow(card, 12., 20., t.shadow);
        ui.p.bordered(card, 12., t.surface, 1., t.line);
        ui.p.hitbox(card, Id::of("pull-card"), CursorIcon::Default);
        let mut r = card;
        let head = r.cut_top(52.);
        let foot = r.cut_bottom(36.);
        ui.p.border_bottom(head, t.line);
        ui.p.fill(foot.with_h(1.), t.line);
        // The head: the input, and where to pull from.
        let loc = p.location();
        let loc_label = if loc.id == "custom" && loc.host.is_empty() {
            d.t("pull.myRegistry")
        } else {
            loc.label.clone()
        };
        let lstyle = ui.style().with_size(12.5).weight(600);
        let dot_w = if loc.account.is_some() { 12. } else { 0. };
        // The registry's mark (16), the login dot, the label, the chevron.
        let lw = ui.p.layout(&loc_label, lstyle, Some(180.)).width + 60. + dot_w;
        let lb = Rect::new(head.right() - 10. - lw, head.y + 12., lw, 28.);
        let searching = loc.id == "dockerhub" && p.supported;
        let placeholder = if searching {
            d.t("pull.searchPlaceholder")
        } else {
            d.tv("pull.refPlaceholder", &[("name", &loc.label)])
        };
        p.input.set_placeholder(&placeholder);
        let input_r = Rect::new(head.x + 14., head.y + 10., lb.x - head.x - 24., 32.);
        if plain_field(ui, &mut p.input, input_r, Some(Icon::Search.id()), !busy) {
            p.on_input(d.jobs.inline());
        }
        let hovered = ui.hovered(LOCATION) && !busy;
        ui.p.bordered(lb, 7., if hovered { t.hover } else { t.sunken }, 1., t.line);
        let mut lx = lb.x + 10.;
        crate::images::draw_registry_icon(ui, loc.id, Rect::new(lx, lb.y + 6., 16., 16.), t.ink);
        lx += 22.;
        if loc.account.is_some() {
            ui.p.rounded(Rect::new(lx, lb.y + 10., 8., 8.), 4., t.green);
            lx += dot_w;
        }
        let fg = if busy { t.ink.with_alpha(0.45) } else { t.ink };
        ui.p.label(
            Rect::new(lx, lb.y, lb.right() - 22. - lx, lb.h),
            &loc_label,
            lstyle,
            fg,
        );
        ui.p.icon(
            icons::CHEVRON_DOWN,
            Rect::new(lb.right() - 19., lb.y + 7., 13., 13.),
            t.ink2,
        );
        if !busy {
            ui.p.hitbox(lb, LOCATION, CursorIcon::Pointer);
            ui.focusable(LOCATION, podbag_ui::FocusKind::Activate, lb, 7.);
            let label = format!("{}: {}", d.t("pull.location"), loc.label);
            ui.tooltip(LOCATION, lb, &label);
            if ui.clicked(LOCATION) {
                p.menu_open = !p.menu_open;
                p.menu_search.set_content("");
                p.focus = Some(if p.menu_open { MENU_SEARCH } else { LOCATION });
            }
        }
        // The body.
        let body = r.inset(8., 8., 8., 8.);
        match p.step {
            Step::Search if !p.menu_open => search_body(d, ui, &mut p, body),
            Step::Search => {}
            Step::Tag => tag_body(d, ui, &mut p, body),
            Step::Host => host_body(d, ui, &mut p, body),
            Step::Login => crate::views::login::body(d, ui, &mut p, body),
            Step::Pulling | Step::Done => {
                if progress_body(d, ui, &mut p, body) {
                    close = true;
                }
            }
        }
        // The foot: the keys.
        let style = ui.style().with_size(12.);
        let keys = format!(
            "↑ ↓ {}   Enter {}   Esc {}",
            d.t("pull.keyMove"),
            d.t("pull.keyPull"),
            d.t("pull.keyClose")
        );
        ui.p.label(foot.inset(12., 0., 12., 0.), &keys, style, t.muted);
        let anywhere = format!(
            "{} {}",
            if cfg!(target_os = "macos") {
                "⌘⇧P"
            } else {
                "Ctrl+Shift+P"
            },
            d.t("pull.keyAnywhere")
        );
        let aw = ui.p.layout(&anywhere, style, None).width;
        ui.p.label(
            Rect::new(foot.right() - 12. - aw, foot.y, aw + 2., foot.h),
            &anywhere,
            style,
            t.muted,
        );
        if p.menu_open {
            location_menu(d, ui, &mut p, lb, card);
        }
    });
    if !close {
        d.pull = Some(p);
    }
}

fn note(ui: &mut Ui, body: Rect, y: &mut f32, text: &str, color: Color) {
    let t = ui.theme;
    let style = ui.style().with_size(13.);
    let h = ui.p.layout_wrapped(text, style, body.w - 40.).height + 20.;
    let r = Rect::new(body.x + 8., *y + 8., body.w - 16., h);
    ui.p.rounded(r, 8., t.sunken);
    ui.paragraph(r.x + 12., r.y + 10., r.w - 24., text, style, color);
    *y += h + 8.;
}

fn search_body(d: &mut Desktop, ui: &mut Ui, p: &mut PullPanel, body: Rect) {
    let t = ui.theme;
    let options = p.options();
    let mut y = body.y;
    if !p.supported && p.location == "dockerhub" {
        note(ui, body, &mut y, &d.t("pull.noSearch"), t.ink2);
    }
    if let Some(e) = p.search_error.clone() {
        note(ui, body, &mut y, &e, t.red);
    }
    if options.is_empty() {
        let key = if p.location == "dockerhub" && p.supported {
            "pull.hint"
        } else {
            "pull.hintRef"
        };
        note(ui, body, &mut y, &d.t(key), t.ink2);
        return;
    }
    if p.active >= options.len() {
        p.active = 0;
    }
    // The arrows and Enter work in the input (a combobox).
    let in_input = ui.has_focus(INPUT);
    if in_input && ui.key_pressed(NamedKey::ArrowDown).is_some() {
        p.active = (p.active + 1) % options.len();
    }
    if in_input && ui.key_pressed(NamedKey::ArrowUp).is_some() {
        p.active = (p.active + options.len() - 1) % options.len();
    }
    let mut chosen = (in_input && ui.key_pressed(NamedKey::Enter).is_some()).then_some(p.active);
    let name_style = ui.style().weight(600);
    let desc_style = ui.style().with_size(12.5);
    ui.clip(body, |ui| {
        for (i, o) in options.iter().enumerate() {
            let r = &o.result;
            let h = if r.description.is_empty() { 40. } else { 54. };
            let row = Rect::new(body.x, y, body.w, h);
            y += h + 1.;
            let id = Id::of(("pull-option", i));
            if ui.hovered(id) && p.active != i && ui.pointer().is_some() {
                p.active = i;
                ui.repaint();
            }
            let on = i == p.active;
            if on {
                ui.p.rounded(row, 8., t.accent_soft);
            }
            let fg = if on { t.accent_text } else { t.ink2 };
            let icon = if o.typed { Icon::Pull } else { Icon::Images };
            ui.p.icon(
                icon.id(),
                Rect::new(row.x + 10., row.y + (row.h - 18.) / 2., 18., 18.),
                fg,
            );
            let mut right = row.right() - 10.;
            if !o.typed || r.star_count > 0 {
                let stars = r.star_count.to_string();
                let sl = ui.p.layout(&stars, ui.style().with_size(12.), None);
                right -= sl.width;
                ui.p.draw_text(&sl, right, row.y + (row.h - sl.height) / 2., t.muted);
                right -= 17.;
                ui.p.icon(
                    Icon::Star.id(),
                    Rect::new(right, row.y + (row.h - 13.) / 2., 13., 13.),
                    t.muted,
                );
                right -= 10.;
            }
            let text = if o.typed {
                d.tv("pull.pullThis", &[("name", &r.name)])
            } else {
                r.name.clone()
            };
            let ty = if r.description.is_empty() {
                row.y + 10.
            } else {
                row.y + 8.
            };
            let nl = ui.p.layout(&text, name_style, Some(right - row.x - 38.));
            ui.p.draw_text(&nl, row.x + 38., ty, t.ink);
            if r.is_official {
                let tag = d.t("pull.official");
                let tl =
                    ui.p.layout(&tag, TextStyle::new(11.).weight(600).line_height(16.), None);
                let tr = Rect::new(row.x + 44. + nl.width, ty + 2., tl.width + 12., 16.);
                ui.p.rounded(tr, 4., t.green_soft);
                ui.p.draw_text(&tl, tr.x + 6., tr.y, t.green);
            }
            if !r.description.is_empty() {
                ui.p.label(
                    Rect::new(row.x + 38., ty + 22., right - row.x - 38., 18.),
                    &r.description,
                    desc_style,
                    t.ink2,
                );
            }
            ui.p.hitbox(row, id, CursorIcon::Pointer);
            if ui.clicked(id) {
                chosen = Some(i);
            }
        }
    });
    if let Some(i) = chosen {
        let o = options[i].clone();
        if o.typed {
            start_pull(d, p, o.result.name);
        } else {
            p.chosen = Some(o.result);
            p.tag.set_content("latest");
            p.tag.select_all();
            p.step = Step::Tag;
            p.focus = Some(TAG);
        }
    }
}

fn form_label(ui: &mut Ui, r: Rect, text: &str) {
    let style = ui.style().with_size(13.).weight(600);
    let ink = ui.theme.ink;
    ui.p.label(r, text, style, ink);
}

fn tag_body(d: &mut Desktop, ui: &mut Ui, p: &mut PullPanel, body: Rect) {
    let name = p
        .chosen
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let r = body.inset(8., 8., 8., 0.);
    form_label(ui, r.with_h(20.), &d.tv("pull.tagFor", &[("name", &name)]));
    text_field(
        ui,
        &mut p.tag,
        Rect::new(r.x, r.y + 28., r.w, 34.),
        None,
        true,
    );
    let enter = ui.has_focus(TAG) && ui.key_pressed(NamedKey::Enter).is_some();
    let label = d.t("action.pull");
    let w = button::width(ui, &label, None, false);
    let go = button::button(
        ui,
        TAG_PULL,
        Rect::new(r.right() - w, r.y + 76., w, 32.),
        &label,
        None,
        ButtonKind::Primary,
        true,
    );
    if go || enter {
        let tag = p.tag.content().trim();
        let image = format!("{name}:{}", if tag.is_empty() { "latest" } else { tag });
        start_pull(d, p, image);
    }
}

fn host_body(d: &mut Desktop, ui: &mut Ui, p: &mut PullPanel, body: Rect) {
    let loc = p.location();
    let r = body.inset(8., 8., 8., 0.);
    form_label(
        ui,
        r.with_h(20.),
        &d.tv("pull.host", &[("name", &loc.label)]),
    );
    let example = if loc.host_example.is_empty() {
        "registry.example.com"
    } else {
        loc.host_example
    };
    p.host.set_placeholder(example);
    text_field(
        ui,
        &mut p.host,
        Rect::new(r.x, r.y + 28., r.w, 34.),
        None,
        true,
    );
    let enter = ui.has_focus(HOST) && ui.key_pressed(NamedKey::Enter).is_some();
    let label = d.t("action.save");
    let w = button::width(ui, &label, None, false);
    let save = button::button(
        ui,
        HOST_SAVE,
        Rect::new(r.right() - w, r.y + 76., w, 32.),
        &label,
        None,
        ButtonKind::Primary,
        true,
    );
    if save || enter {
        let v = p.host.content().trim().to_string();
        if v.is_empty() {
            return;
        }
        d.pull_hosts.retain(|(id, _)| id != loc.id);
        d.pull_hosts.push((loc.id.to_string(), v));
        p.locations = d.api.registries(&d.pull_hosts);
        if p.after_host.take() == Some(Step::Login) {
            p.step = Step::Login;
            p.focus = Some(crate::views::login::USER);
        } else {
            p.step = Step::Search;
            p.focus = Some(INPUT);
        }
    }
}

/// Returns true to close the panel.
fn progress_body(d: &mut Desktop, ui: &mut Ui, p: &mut PullPanel, body: Rect) -> bool {
    let t = ui.theme;
    let r = body.inset(8., 8., 8., 0.);
    if p.step == Step::Pulling {
        let title = d.tv("pull.pulling", &[("name", &p.pulling)]);
        let h = ui.paragraph(r.x, r.y, r.w, &title, ui.style().weight(600), t.ink);
        let log = Rect::new(r.x, r.y + h + 8., r.w, (r.h - h - 16.).clamp(40., 260.));
        ui.p.rounded(log, 8., t.sunken);
        let mono = TextStyle::new(12.).mono().line_height(18.);
        let start = p.lines.len().saturating_sub(12);
        let text = p.lines[start..].join("\n");
        ui.clip(log, |ui| {
            ui.paragraph(log.x + 12., log.y + 10., log.w - 24., &text, mono, t.ink)
        });
        return false;
    }
    let (ok, text) = match &p.result {
        Some(Ok(name)) => (true, d.tv("done.pull", &[("name", name)])),
        Some(Err(e)) => (false, e.clone()),
        None => (true, String::new()),
    };
    let style = if ok {
        ui.style().weight(600)
    } else {
        ui.style()
    };
    let h = ui.paragraph(r.x, r.y, r.w, &text, style, if ok { t.ink } else { t.red });
    let close_label = d.t("pull.close");
    let another = d.t("pull.another");
    let cw = button::width(ui, &close_label, None, false);
    let aw = button::width(ui, &another, None, false);
    let y = r.y + h + 16.;
    if button::button(
        ui,
        ANOTHER,
        Rect::new(r.right() - cw - 8. - aw, y, aw, 32.),
        &another,
        None,
        ButtonKind::Normal,
        true,
    ) {
        p.step = Step::Search;
        p.result = None;
        p.input.set_content("");
        p.results.clear();
        p.focus = Some(INPUT);
    }
    button::button(
        ui,
        CLOSE,
        Rect::new(r.right() - cw, y, cw, 32.),
        &close_label,
        None,
        ButtonKind::Primary,
        true,
    )
}

fn location_menu(d: &mut Desktop, ui: &mut Ui, p: &mut PullPanel, button_r: Rect, card: Rect) {
    let t = ui.theme;
    let w = (card.w - 20.).min(400.);
    let h = (ui.bounds().bottom() - button_r.bottom() - 40.).min(420.);
    let menu = Rect::new(card.right() - 10. - w, button_r.bottom() + 8., w, h);
    let enter = ui.has_focus(MENU_SEARCH) && ui.key_pressed(NamedKey::Enter).is_some();
    let mut pick = None;
    let mut login = None;
    let login_label = d.t("pull.login");
    ui.on_layer(layer::MODAL_POPUP, |ui| {
        ui.p.shadow(menu, 10., 14., t.shadow);
        ui.p.bordered(menu, 10., t.surface, 1., t.line);
        ui.p.hitbox(menu, Id::of("loc-menu"), CursorIcon::Default);
        let inner = menu.inset(6., 6., 6., 6.);
        let title_style = ui.style().with_size(11.5).weight(600);
        ui.p.label(
            Rect::new(inner.x + 8., inner.y + 2., inner.w, 18.),
            &d.t("pull.location"),
            title_style,
            t.muted,
        );
        p.menu_search.set_placeholder(&d.t("search.placeholder"));
        text_field(
            ui,
            &mut p.menu_search,
            Rect::new(inner.x + 4., inner.y + 24., inner.w - 8., 30.),
            None,
            true,
        );
        let terms: Vec<String> = p
            .menu_search
            .content()
            .trim()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let list = Rect::new(inner.x, inner.y + 60., inner.w, inner.h - 60.);
        let mut y = list.y;
        let label_style = ui.style().with_size(13.5);
        let mut first = None;
        ui.clip(list, |ui| {
            for l in &p.locations {
                let label = if l.needs_host && !l.host.is_empty() {
                    format!("{} ({})", l.label, l.host)
                } else if l.id == "custom" {
                    d.t("pull.myRegistry")
                } else {
                    l.label.clone()
                };
                let hay = [label.as_str(), l.id, l.host.as_str(), l.host_example]
                    .join(" ")
                    .to_lowercase();
                if !terms.iter().all(|t| hay.contains(t)) && l.id != "custom" {
                    continue;
                }
                first.get_or_insert(l.id);
                let r = Rect::new(list.x, y, list.w, 32.);
                y += 33.;
                if r.y > list.bottom() {
                    continue;
                }
                let id = Id::of(("loc", l.id));
                let chosen = l.id == p.location;
                // Without an account: a "Log in" button at the row's right.
                let login_w = if l.account.is_none() {
                    button::width(ui, &login_label, None, true)
                } else {
                    0.
                };
                let row = r;
                let r = if login_w > 0. {
                    r.inset(0., 0., login_w + 8., 0.)
                } else {
                    r
                };
                if chosen || ui.hovered(id) {
                    ui.p.rounded(r, 7., t.hover);
                }
                if login_w > 0. {
                    let b = Rect::new(row.right() - login_w - 2., row.y + 3., login_w, 26.);
                    let lid = crate::views::login::menu_login_id(l.id);
                    if button::button(ui, lid, b, &login_label, None, ButtonKind::Normal, true) {
                        login = Some(l.id);
                    }
                    ui.tooltip(lid.with("tip"), b, &format!("{login_label}: {label}"));
                }
                crate::images::draw_registry_icon(
                    ui,
                    l.id,
                    Rect::new(r.x + 9., r.y + 6., 20., 20.),
                    t.ink,
                );
                ui.label(r.inset(36., 0., 30., 0.), &label, label_style, t.ink);
                if let Some(acc) = &l.account {
                    let al = ui.p.layout(acc, ui.style().with_size(12.), None);
                    ui.p.draw_text(
                        &al,
                        r.right() - 34. - al.width,
                        r.y + (r.h - al.height) / 2.,
                        t.ink2,
                    );
                }
                if chosen {
                    ui.p.icon(
                        icons::CHECK,
                        Rect::new(r.right() - 24., r.y + 9., 15., 15.),
                        t.accent_text,
                    );
                }
                ui.p.hitbox(r, id, CursorIcon::Pointer);
                ui.focusable(id, podbag_ui::FocusKind::Activate, r, 7.);
                if ui.clicked(id) {
                    pick = Some(l.id);
                }
            }
        });
        if enter {
            pick = pick.or(first);
        }
    });
    if let Some(id) = login {
        crate::views::login::start(p, id);
        return;
    }
    let Some(id) = pick else { return };
    let Some(l) = p.locations.iter().find(|l| l.id == id).cloned() else {
        return;
    };
    p.location = l.id.to_string();
    p.results.clear();
    p.menu_open = false;
    if l.needs_host {
        p.after_host = Some(Step::Search);
        p.host.set_content(&l.host);
        p.step = Step::Host;
        p.focus = Some(HOST);
    } else {
        p.on_input(d.jobs.inline());
        p.focus = Some(INPUT);
    }
}
