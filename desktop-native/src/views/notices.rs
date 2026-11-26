//! The first-launch star prompt, and the notices in the corner (app.js `notice`): the
//! `podbag` command for terminals, and an update of the terminal's older `podbag`.
//! Also the settings section of the terminal command.

use podbag_desktop_core::terminal_cli::Status;
use podbag_desktop_core::updates::Check;
use podbag_ui::layer;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind as ButtonKind};
use podbag_ui::widgets::dialog::buttons_row;
use podbag_ui::{CursorIcon, Id, Rect};

use crate::app::Desktop;
use crate::dialogs::Dialog;
use crate::icons::Icon;
use crate::views::blocks::{Buttons, small, spawn};
use crate::views::{field, fields, updates};

pub const CLI_LATER: Id = Id(0x6e6f_7469_6301);
pub const CLI_INSTALL: Id = Id(0x6e6f_7469_6302);
pub const UPDATE_LATER: Id = Id(0x6e6f_7469_6303);
pub const UPDATE_NOW: Id = Id(0x6e6f_7469_6304);
pub const SETTINGS_INSTALL: Id = Id(0x6e6f_7469_6305);

/// A notice that shows: its error line, and whether its work runs.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Notice {
    pub status: String,
    pub busy: bool,
}

#[derive(Default)]
pub struct Notices {
    /// What terminals run for `podbag`; None: this app has no podbag to offer.
    pub cli: Option<Status>,
    pub cli_notice: Option<Notice>,
    pub update_notice: Option<Notice>,
    /// The install in the settings: its error, and whether it runs.
    pub settings: Notice,
}

/// At start (app.js `showFirstLaunchStarPrompt().then(showTerminalCommandNotice)`).
pub fn start(d: &mut Desktop) {
    if d.api.take_star_prompt() {
        d.open_dialog(Dialog::Star);
    } else {
        terminal_notice(d);
    }
}

/// The star prompt was answered: open the repository on "Open GitHub", then go on.
pub fn star_answered(d: &mut Desktop, open: bool) {
    if open && d.api.open_repository().is_err() {
        let text = d.t("star.openFailed");
        d.toasts.push(text, true);
    }
    terminal_notice(d);
}

/// app.js `showTerminalCommandNotice`: a packaged app that finds no `podbag` for
/// terminals offers its own, until "Not now".
pub fn terminal_notice(d: &mut Desktop) {
    spawn(
        d,
        |api| api.terminal_command(),
        |d, cli| {
            let show = cli
                .as_ref()
                .is_some_and(|c| c.state == "missing" && c.problem.is_none())
                && !d.settings.cli_notice_dismissed;
            d.notices.cli = cli;
            if show {
                d.notices.cli_notice = Some(Notice::default());
            }
        },
    );
}

/// app.js `showTerminalUpdateNotice`: the terminal's separate `podbag` is older than
/// the app. Called when the versions come in.
pub fn terminal_update(d: &mut Desktop) {
    let u = d.updates.clone().unwrap_or_default();
    let wanted = u.installer.is_some()
        && u.terminal_older
        && d.settings.cli_update_dismissed.as_deref() != Some(u.app.as_str());
    if !wanted {
        d.notices.update_notice = None;
    } else if d.notices.update_notice.is_none() && !d.update.upgrading {
        d.notices.update_notice = Some(Notice::default());
    }
}

/// Where Install puts the command.
fn where_text(d: &Desktop, c: &Status) -> String {
    let key = if cfg!(windows) {
        "cli.wherePath"
    } else {
        "cli.whereLink"
    };
    d.tv(key, &[("path", &c.target)])
}

/// app.js `installTerminalCommand`: make the command. `from_notice`: the notice shows
/// the error; else the settings.
fn install(d: &mut Desktop, from_notice: bool) {
    let busy = if from_notice {
        d.notices.cli_notice.as_mut().map(|n| &mut n.busy)
    } else {
        Some(&mut d.notices.settings.busy)
    };
    match busy {
        Some(b) if !*b => *b = true,
        _ => return,
    }
    if from_notice {
        if let Some(n) = &mut d.notices.cli_notice {
            n.status.clear();
        }
    } else {
        d.notices.settings.status.clear();
    }
    spawn(
        d,
        |api| api.install_terminal_command(),
        move |d, r| {
            d.notices.settings.busy = false;
            if let Some(n) = &mut d.notices.cli_notice {
                n.busy = false;
            }
            match r {
                Ok(Some(status)) => {
                    d.notices.cli = Some(status);
                    d.notices.cli_notice = None;
                    let text = d.t("cli.done");
                    d.toasts.push(text, false);
                }
                // The password was not given.
                Ok(None) => {}
                Err(e) => {
                    let text = d.tv("cli.failed", &[("error", &e)]);
                    match &mut d.notices.cli_notice {
                        Some(n) if from_notice => n.status = text,
                        _ => d.notices.settings.status = text,
                    }
                }
            }
        },
    );
}

fn update_terminal(d: &mut Desktop) {
    let Some(n) = &mut d.notices.update_notice else {
        return;
    };
    if n.busy {
        return;
    }
    n.busy = true;
    let version = d
        .updates
        .as_ref()
        .map(|u| u.app.clone())
        .unwrap_or_default();
    spawn(
        d,
        {
            let version = version.clone();
            move |api| api.upgrade(Some(&version))
        },
        move |d, r| {
            d.notices.update_notice = None;
            if r.ok {
                let text = d.tv("cliUpdate.done", &[("version", &version)]);
                d.toasts.push(text, false);
            } else {
                // The password or an error: Updates shows the command and the output.
                d.update.upgrade = Some(r);
                d.open_section("updates");
            }
            updates::load(d, Check::Never);
        },
    );
}

// ---- painting ----

/// One notice card at the bottom right, its bottom at `bottom`. Returns its top and
/// which of its two buttons was clicked (0: the first).
#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    id: Id,
    bottom: f32,
    title: &str,
    body: &str,
    status: &str,
    buttons: [(Id, &str, Option<Icon>, ButtonKind, bool); 2],
) -> (f32, Option<usize>) {
    let t = ui.theme;
    let bounds = ui.bounds();
    let w = 420f32.min(bounds.w - 36.);
    let inner = w - 32.;
    let title_style = ui.style().with_size(14.).weight(700);
    let text_style = ui.style().with_size(13.5);
    let th = ui.p.layout_wrapped(title, title_style, inner).height;
    let bh = ui.p.layout_wrapped(body, text_style, inner).height;
    let sh = if status.is_empty() {
        0.
    } else {
        ui.p.layout_wrapped(status, text_style, inner).height + 8.
    };
    let h = 14. + th + 8. + bh + 8. + sh + 4. + 32. + 14.;
    let r = Rect::new(bounds.right() - 18. - w, bottom - h, w, h);
    let mut clicked = None;
    ui.on_layer(layer::POPUP, |ui| {
        ui.p.shadow(r, 10., 14., t.shadow);
        ui.p.bordered(r, 10., t.surface, 1., t.line);
        ui.p.hitbox(r, id, CursorIcon::Default);
        let x = r.x + 16.;
        let mut y = r.y + 14.;
        y += ui.paragraph(x, y, inner, title, title_style, t.ink) + 8.;
        y += ui.paragraph(x, y, inner, body, text_style, t.ink) + 8.;
        if !status.is_empty() {
            y += ui.paragraph(x, y, inner, status, text_style, t.red) + 8.;
        }
        y += 4.;
        let widths: Vec<f32> = buttons
            .iter()
            .rev()
            .map(|(_, l, i, _, _)| button::width(ui, l, i.map(Icon::id), false))
            .collect();
        let rects = buttons_row(x, y, inner, &widths);
        for (n, (bid, label, icon, kind, enabled)) in buttons.iter().enumerate() {
            let rect = rects[buttons.len() - 1 - n];
            if button::button(ui, *bid, rect, label, icon.map(Icon::id), *kind, *enabled) {
                clicked = Some(n);
            }
        }
    });
    (r.y, clicked)
}

/// The notices that show, above the status bar.
pub fn paint(d: &mut Desktop, ui: &mut Ui) {
    let mut bottom = ui.bounds().bottom() - 27. - 18.;
    if let (Some(n), Some(c)) = (d.notices.cli_notice.clone(), d.notices.cli.clone()) {
        let body = format!("{} {}", d.t("cli.text"), where_text(d, &c));
        let install_label = d.t(if n.busy {
            "cli.installing"
        } else {
            "cli.install"
        });
        let later = d.t("cli.later");
        let (top, clicked) = card(
            ui,
            Id::of("cli-notice"),
            bottom,
            &d.t("cli.title"),
            &body,
            &n.status,
            [
                (CLI_LATER, &later, None, ButtonKind::Normal, true),
                (
                    CLI_INSTALL,
                    &install_label,
                    Some(Icon::Shell),
                    ButtonKind::Primary,
                    !n.busy,
                ),
            ],
        );
        bottom = top - 8.;
        match clicked {
            Some(0) => {
                d.notices.cli_notice = None;
                d.settings.cli_notice_dismissed = true;
                d.save_settings();
                let text = d.t("cli.laterNote");
                d.toasts.push(text, false);
            }
            Some(_) => install(d, true),
            None => {}
        }
    }
    if let Some(n) = d.notices.update_notice.clone() {
        let u = d.updates.clone().unwrap_or_default();
        let body = d.tv(
            "cliUpdate.text",
            &[
                ("terminal", u.terminal_version.as_deref().unwrap_or("?")),
                ("app", &u.app),
                ("path", u.terminal.as_deref().unwrap_or_default()),
            ],
        );
        let update_label = d.t(if n.busy {
            "update.installing"
        } else {
            "cliUpdate.update"
        });
        let later = d.t("cli.later");
        let (_, clicked) = card(
            ui,
            Id::of("cli-update-notice"),
            bottom,
            &d.t("cliUpdate.title"),
            &body,
            &n.status,
            [
                (UPDATE_LATER, &later, None, ButtonKind::Normal, true),
                (
                    UPDATE_NOW,
                    &update_label,
                    Some(Icon::Update),
                    ButtonKind::Primary,
                    !n.busy,
                ),
            ],
        );
        match clicked {
            Some(0) => {
                d.notices.update_notice = None;
                d.settings.cli_update_dismissed = Some(u.app.clone());
                d.save_settings();
            }
            Some(_) => update_terminal(d),
            None => {}
        }
    }
}

// ---- the settings section ----

/// The section's value: what terminals run. None: the section is hidden.
pub fn value(d: &Desktop) -> Option<String> {
    let c = d.notices.cli.as_ref()?;
    Some(d.t(&format!("cli.state.{}", c.state)))
}

/// app.js `renderTerminalCommand`; returns the height.
pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let Some(c) = d.notices.cli.clone() else {
        return 0.;
    };
    let mut y = y0;
    y += small(
        ui,
        x,
        y,
        w,
        &d.t(&format!("cli.about.{}", c.state)),
        t.muted,
    ) + 10.;
    if let Some(path) = &c.path {
        let rows = [field(d.t("cli.path"), path.clone(), true)];
        let none = d.t("detail.none");
        y += fields(ui, Id::of("cli-fields"), x, y, w, &rows, &none);
    }
    if c.state == "missing" {
        if let Some(problem) = c.problem {
            y += small(ui, x, y, w, &d.t(&format!("cli.problem.{problem}")), t.red) + 10.;
        } else {
            y += small(ui, x, y, w, &where_text(d, &c), t.muted) + 10.;
            let busy = d.notices.settings.busy;
            let mut row = Buttons::new(x, y, w);
            let label = d.t(if busy {
                "cli.installing"
            } else {
                "cli.install"
            });
            if row.add(
                ui,
                SETTINGS_INSTALL,
                &label,
                Some(Icon::Shell),
                ButtonKind::Primary,
                !busy,
            ) {
                install(d, false);
            }
            y += row.height(y) + 8.;
            let status = d.notices.settings.status.clone();
            if !status.is_empty() {
                y += small(ui, x, y, w, &status, t.red) + 8.;
            }
        }
    }
    y + 8. - y0
}
