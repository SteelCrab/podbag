//! Logging in to a registry from the pull panel (images.js `loginBody`): a user and a
//! password, or for a cloud registry the command its own tool logs in with. The
//! password field shows dots and keeps the text out of the clipboard.

use podbag_ui::TextInput;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind as ButtonKind};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{Id, Rect};

use crate::app::Desktop;
use crate::views::blocks::{code_box, input, small, spawn};
use crate::views::pull::{self, PullPanel, Step};

pub const USER: Id = Id(0x6c6f_6769_6e01);
pub const PASSWORD: Id = Id(0x6c6f_6769_6e02);
pub const SUBMIT: Id = Id(0x6c6f_6769_6e03);

/// The menu's "Log in" button of a location.
pub fn menu_login_id(location: &str) -> Id {
    Id::of(("loc-login", location))
}

const DOT: char = '•';

/// A password field: the input shows a dot for each character; the text is kept apart.
/// Editing works at the end (typing, pasting, Backspace) and on the whole text.
pub struct Secret {
    pub input: TextInput,
    text: Vec<char>,
}

impl Secret {
    pub fn new(id: Id) -> Secret {
        Secret {
            input: input(id, ""),
            text: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The text, which the field forgets.
    pub fn take(&mut self) -> String {
        self.input.set_content("");
        std::mem::take(&mut self.text).into_iter().collect()
    }

    /// Paints the field; true when the text changed.
    pub fn field(&mut self, ui: &mut Ui, rect: Rect, enabled: bool) -> bool {
        let len = self.input.content().len();
        let sel = self.input.edit.selected.clone();
        let all = sel.start == 0 && sel.end == len;
        if !all && (!sel.is_empty() || self.input.edit.cursor() != len) {
            self.input.edit.move_to(len);
        }
        if !text_field(ui, &mut self.input, rect, None, enabled) {
            return false;
        }
        let shown = self.input.content();
        let dots = shown.chars().filter(|c| *c == DOT).count();
        let typed: Vec<char> = shown.chars().filter(|c| *c != DOT).collect();
        self.text.truncate(dots);
        self.text.extend(typed);
        let masked: String = std::iter::repeat_n(DOT, self.text.len()).collect();
        self.input.set_content(&masked);
        self.input.edit.move_to(masked.len());
        true
    }
}

/// The login fields of the panel.
pub struct Login {
    pub user: TextInput,
    pub password: Secret,
    pub status: String,
    pub busy: bool,
}

impl Default for Login {
    fn default() -> Self {
        Login {
            user: input(USER, ""),
            password: Secret::new(PASSWORD),
            status: String::new(),
            busy: false,
        }
    }
}

/// The location menu's "Log in": the host first when the location needs one.
pub fn start(p: &mut PullPanel, location: &str) {
    p.location = location.to_string();
    p.menu_open = false;
    p.login = Login::default();
    let loc = p.current();
    p.after_host = Some(Step::Login);
    if loc.needs_host && loc.host.is_empty() {
        p.host.set_content("");
        p.step = Step::Host;
        p.focus = Some(pull::HOST);
    } else {
        p.step = Step::Login;
        p.focus = Some(USER);
    }
}

fn submit(d: &mut Desktop, p: &mut PullPanel) {
    let user = p.login.user.content().trim().to_string();
    if user.is_empty() || p.login.password.is_empty() || p.login.busy {
        return;
    }
    let loc = p.current();
    p.login.busy = true;
    p.login.status = d.t("pull.loggingIn");
    let password = p.login.password.take();
    let (id, host, name) = (loc.id, loc.host.clone(), loc.label.clone());
    spawn(
        d,
        move |api| api.registry_login(id, &host, &user, &password),
        move |d, r| {
            let Some(p) = &mut d.pull else { return };
            match r {
                Ok(()) => {
                    p.locations = d.api.registries(&d.pull_hosts);
                    p.login = Login::default();
                    p.step = Step::Search;
                    p.focus = Some(pull::INPUT);
                    let text = d.tv("pull.loggedIn", &[("name", &name)]);
                    d.toasts.push(text, false);
                }
                Err(e) => {
                    p.login.busy = false;
                    p.login.status = e;
                    p.focus = Some(PASSWORD);
                }
            }
        },
    );
}

fn label(ui: &mut Ui, r: Rect, text: &str) {
    let style = ui.style().with_size(13.).weight(600);
    let ink = ui.theme.ink;
    ui.p.label(r, text, style, ink);
}

/// The panel's body at the login step.
pub fn body(d: &mut Desktop, ui: &mut Ui, p: &mut PullPanel, body: Rect) {
    let t = ui.theme;
    let loc = p.current();
    let r = body.inset(8., 8., 8., 0.);
    let mut y = r.y;
    if let Some(hint) = &loc.hint {
        let text = d.tv("pull.loginCloud", &[("name", &loc.label)]);
        y += ui.paragraph(r.x, y, r.w, &text, ui.style(), t.ink) + 10.;
        code_box(d, ui, Id::of("login-hint"), r.x, y, r.w, hint);
        return;
    }
    let busy = p.login.busy;
    let title = d.tv("pull.loginTo", &[("name", &loc.label)]);
    y += ui.paragraph(r.x, y, r.w, &title, ui.style().weight(600), t.ink) + 10.;
    label(ui, Rect::new(r.x, y, r.w, 18.), &d.t("pull.user"));
    y += 24.;
    text_field(
        ui,
        &mut p.login.user,
        Rect::new(r.x, y, r.w, 34.),
        None,
        !busy,
    );
    let mut enter = ui.has_focus(USER) && ui.key_pressed(NamedKey::Enter).is_some();
    y += 42.;
    label(ui, Rect::new(r.x, y, r.w, 18.), &d.t("pull.password"));
    y += 24.;
    p.login
        .password
        .field(ui, Rect::new(r.x, y, r.w, 34.), !busy);
    enter |= ui.has_focus(PASSWORD) && ui.key_pressed(NamedKey::Enter).is_some();
    y += 40.;
    y += small(ui, r.x, y, r.w, &d.t("pull.passwordNote"), t.muted) + 6.;
    if !p.login.status.is_empty() {
        let color = if busy { t.ink2 } else { t.red };
        y += small(ui, r.x, y, r.w, &p.login.status.clone(), color) + 6.;
    }
    let text = d.t("pull.login");
    let w = button::width(ui, &text, None, false);
    let go = button::button(
        ui,
        SUBMIT,
        Rect::new(r.right() - w, y + 4., w, 32.),
        &text,
        None,
        ButtonKind::Primary,
        !busy,
    );
    if go || enter {
        submit(d, p);
    }
}
