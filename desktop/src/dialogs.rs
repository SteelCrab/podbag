//! The modal dialogs: confirm, ask (one line of text that runs work while the dialog
//! stays open), choose (several answers), the Kubernetes object viewer, the star prompt
//! and the question about image compression; and the dialogs of Migrate, of the import
//! from Docker and the run form, which their views paint. Escape closes them; Tab stays inside
//! them.

use podbag_desktop_core::Kind;
use podbag_desktop_core::vm::VmSettings;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind as ButtonKind};
use podbag_ui::widgets::dialog::{buttons_row, dialog};
use podbag_ui::widgets::logs::{LogView, log_view};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{Color, Id, Rect, TextInput, TextStyle};

use crate::app::Desktop;
use crate::tr::Tr;

/// What a confirmed dialog does.
pub enum Then {
    Remove {
        kind: Kind,
        id: String,
        name: String,
        force: bool,
    },
    Prune(Kind),
    Vm(&'static str),
    /// Switch the docker command: `use` or `back`.
    Docker(&'static str),
    /// Put the text on the clipboard.
    Copy(String),
    /// Install a module file's text (`module`: its id; None: the JSON lesson).
    InstallModule {
        module: Option<String>,
        text: String,
    },
    RemoveModule(String),
    /// End the JSON lesson's lab.
    EndLab,
    /// Remove a practice volume in the practice session.
    PracticeRemove {
        name: String,
        session: String,
    },
    /// Stop synchronizing a share of the macOS VM.
    SyncRemove(String),
    RemoveBuilder {
        session: u64,
        scope: String,
        name: String,
    },
}

/// What an ask dialog's text is for.
pub enum AskFor {
    Rename(String),
    CreateVolume,
    Socket,
    /// The path of a training module file.
    ModuleFile,
}

pub enum Dialog {
    Confirm {
        title: String,
        text: String,
        yes: String,
        cancel: String,
        danger: bool,
        then: Then,
    },
    Ask {
        title: String,
        label: String,
        input: TextInput,
        ok: String,
        error: String,
        working: bool,
        then: AskFor,
    },
    /// The VM's new resources: now (restart), at the next start, or not.
    Choose {
        title: String,
        text: String,
        danger: bool,
        change: VmSettings,
    },
    KubeObject {
        title: String,
        context: String,
        lines: LogView,
    },
    /// The first launch: star the project on GitHub?
    Star,
    /// Once: turn on image compression?
    Compression,
    Migration(Box<crate::views::migrate::MigrationDialog>),
    Import(Box<crate::views::import::ImportDialog>),
    Run(Box<crate::views::run::RunForm>),
}

pub const ID: Id = Id(0x6469_616c_6f67);
pub const CANCEL: Id = Id(0x6469_616c_6f68);
pub const YES: Id = Id(0x6469_616c_6f69);
pub const LATER: Id = Id(0x6469_616c_6f6a);
pub const INPUT: Id = Id(0x6469_616c_6f6b);

impl Dialog {
    pub fn confirm(tr: &Tr, text: String, yes: Option<String>, then: Then) -> Dialog {
        Dialog::Confirm {
            title: tr.t("confirm.title"),
            text,
            yes: yes.unwrap_or_else(|| tr.t("action.remove")),
            cancel: tr.t("action.cancel"),
            danger: true,
            then,
        }
    }

    pub fn ask(
        title: String,
        label: String,
        value: &str,
        placeholder: &str,
        ok: String,
        then: AskFor,
    ) -> Dialog {
        let mut input = TextInput::new(
            INPUT,
            placeholder,
            TextStyle::new(14.).line_height(20.),
            Color::TRANSPARENT,
        );
        input.set_content(value);
        input.select_all();
        Dialog::Ask {
            title,
            label,
            input,
            ok,
            error: String::new(),
            working: false,
            then,
        }
    }

    /// The widget that has the focus when the dialog opens.
    pub fn first_focus(&self) -> Id {
        match self {
            Dialog::Ask { .. } => INPUT,
            Dialog::Run(f) => f.first_focus(),
            _ => CANCEL,
        }
    }
}

enum Outcome {
    Open,
    Close,
    Yes,
    Later,
}

pub fn paint(d: &mut Desktop, ui: &mut Ui) {
    let Some(mut dlg) = d.dialog.take() else {
        return;
    };
    let tr = d.tr;
    let t = ui.theme;
    let mut outcome = Outcome::Open;
    let mut submit = None;
    let escape = match &mut dlg {
        Dialog::Migration(m) => {
            if crate::views::migrate::paint_dialog(d, ui, m) {
                outcome = Outcome::Close;
            }
            false
        }
        Dialog::Import(m) => {
            if crate::views::import::paint_dialog(d, ui, m) {
                outcome = Outcome::Close;
            }
            false
        }
        Dialog::Run(f) => {
            if crate::views::run::paint_dialog(d, ui, f) {
                outcome = Outcome::Close;
            }
            false
        }
        Dialog::Star => dialog(ui, ID, 440., |ui, r| {
            let mut y = r.y;
            y += heading(ui, r.x, y, r.w, &tr.t("star.title")) + 10.;
            y += ui.paragraph(r.x, y, r.w, &tr.t("star.text"), ui.style(), t.ink2) + 18.;
            let (skip, open) = (tr.t("star.skip"), tr.t("star.open"));
            let star = Some(crate::icons::Icon::Star.id());
            let widths = [
                button::width(ui, &open, star, false),
                button::width(ui, &skip, None, false),
            ];
            let rects = buttons_row(r.x, y, r.w, &widths);
            if button::button(ui, CANCEL, rects[1], &skip, None, ButtonKind::Normal, true) {
                outcome = Outcome::Close;
            }
            if button::button(ui, YES, rects[0], &open, star, ButtonKind::Primary, true) {
                outcome = Outcome::Yes;
            }
            y + 32. - r.y
        }),
        Dialog::Compression => dialog(ui, ID, 440., |ui, r| {
            let mut y = r.y;
            // The compression icon next to the title.
            let badge = Rect::new(r.x, y, 40., 40.);
            ui.p.rounded(badge, 10., t.accent_soft);
            let icon = crate::icons::Icon::Compress.id();
            ui.p.icon(icon, badge.center(22., 22.), t.accent_text);
            let title = tr.t("compression.title");
            let style = ui.style().with_size(16.).weight(700);
            let th = ui.p.layout_wrapped(&title, style, r.w - 52.).height;
            ui.paragraph(
                r.x + 52.,
                y + (40. - th).max(0.) / 2.,
                r.w - 52.,
                &title,
                style,
                t.ink,
            );
            y += th.max(40.) + 12.;
            let text = tr.t("compression.text");
            y += ui.paragraph(r.x, y, r.w, &text, ui.style(), t.ink2) + 18.;
            let (later, on) = (tr.t("compression.later"), tr.t("compression.turnOn"));
            let widths = [
                button::width(ui, &on, None, false),
                button::width(ui, &later, None, false),
            ];
            let rects = buttons_row(r.x, y, r.w, &widths);
            if button::button(ui, CANCEL, rects[1], &later, None, ButtonKind::Normal, true) {
                outcome = Outcome::Close;
            }
            if button::button(ui, YES, rects[0], &on, None, ButtonKind::Primary, true) {
                outcome = Outcome::Yes;
            }
            y + 32. - r.y
        }),
        Dialog::Confirm {
            title,
            text,
            yes,
            cancel,
            danger,
            ..
        } => dialog(ui, ID, 440., |ui, r| {
            let mut y = r.y;
            y += heading(ui, r.x, y, r.w, title) + 10.;
            y += ui.paragraph(r.x, y, r.w, text, ui.style(), t.ink2) + 18.;
            let widths = [
                button::width(ui, cancel, None, false),
                button::width(ui, yes, None, false),
            ];
            // Right to left: yes, then cancel; painted (and tabbed) left to right.
            let rects = buttons_row(r.x, y, r.w, &[widths[1], widths[0]]);
            if button::button(ui, CANCEL, rects[1], cancel, None, ButtonKind::Normal, true) {
                outcome = Outcome::Close;
            }
            let kind = if *danger {
                ButtonKind::Danger
            } else {
                ButtonKind::Primary
            };
            if button::button(ui, YES, rects[0], yes, None, kind, true) {
                outcome = Outcome::Yes;
            }
            y + 32. - r.y
        }),
        Dialog::Ask {
            title,
            label,
            input,
            ok,
            error,
            working,
            ..
        } => dialog(ui, ID, 440., |ui, r| {
            let mut y = r.y;
            y += heading(ui, r.x, y, r.w, title) + 12.;
            let style = ui.style().with_size(13.).weight(600);
            ui.p.label(Rect::new(r.x, y, r.w, 18.), label, style, t.ink);
            y += 24.;
            text_field(ui, input, Rect::new(r.x, y, r.w, 34.), None, !*working);
            y += 42.;
            if !error.is_empty() {
                y += ui.paragraph(r.x, y, r.w, error, ui.style().with_size(13.), t.red) + 8.;
            }
            if !*working && ui.has_focus(INPUT) && ui.key_pressed(NamedKey::Enter).is_some() {
                submit = Some(input.content().trim().to_string());
            }
            let cancel = tr.t("action.cancel");
            let widths = [
                button::width(ui, ok, None, false),
                button::width(ui, &cancel, None, false),
            ];
            let rects = buttons_row(r.x, y + 4., r.w, &widths);
            if button::button(
                ui,
                CANCEL,
                rects[1],
                &cancel,
                None,
                ButtonKind::Normal,
                true,
            ) {
                outcome = Outcome::Close;
            }
            if button::button(ui, YES, rects[0], ok, None, ButtonKind::Primary, !*working) {
                submit = Some(input.content().trim().to_string());
            }
            y + 36. - r.y
        }),
        Dialog::Choose {
            title,
            text,
            danger,
            ..
        } => dialog(ui, ID, 460., |ui, r| {
            let mut y = r.y;
            y += heading(ui, r.x, y, r.w, title) + 10.;
            y += ui.paragraph(r.x, y, r.w, text, ui.style(), t.ink2) + 18.;
            let (cancel, later, now) = (
                tr.t("action.cancel"),
                tr.t("settings.nextStart"),
                tr.t("settings.restartNow"),
            );
            let widths = [
                button::width(ui, &now, None, false),
                button::width(ui, &later, None, false),
                button::width(ui, &cancel, None, false),
            ];
            let rects = buttons_row(r.x, y, r.w, &widths);
            if button::button(
                ui,
                CANCEL,
                rects[2],
                &cancel,
                None,
                ButtonKind::Normal,
                true,
            ) {
                outcome = Outcome::Close;
            }
            if button::button(ui, LATER, rects[1], &later, None, ButtonKind::Normal, true) {
                outcome = Outcome::Later;
            }
            let kind = if *danger {
                ButtonKind::Danger
            } else {
                ButtonKind::Primary
            };
            if button::button(ui, YES, rects[0], &now, None, kind, true) {
                outcome = Outcome::Yes;
            }
            y + 32. - r.y
        }),
        Dialog::KubeObject {
            title,
            context,
            lines,
        } => {
            let width = (ui.bounds().w - 48.).min(720.);
            let pre_h = (ui.bounds().h * 0.55).min(lines.lines.len() as f32 * 19. + 26.);
            dialog(ui, ID, width, |ui, r| {
                let mut y = r.y;
                y += heading(ui, r.x, y, r.w, title) + 10.;
                y += ui.paragraph(r.x, y, r.w, context, ui.style(), t.ink2) + 10.;
                log_view(
                    ui,
                    ID.with("json"),
                    Rect::new(r.x, y, r.w, pre_h),
                    lines,
                    "",
                );
                y += pre_h + 16.;
                let back = tr.t("action.back");
                let w = button::width(ui, &back, None, false);
                let rects = buttons_row(r.x, y, r.w, &[w]);
                if button::button(ui, CANCEL, rects[0], &back, None, ButtonKind::Normal, true) {
                    outcome = Outcome::Close;
                }
                y + 32. - r.y
            })
        }
    };
    if escape {
        outcome = Outcome::Close;
    }
    if let Some(text) = submit
        && !text.is_empty()
        && let Dialog::Ask { working, error, .. } = &mut dlg
    {
        *working = true;
        error.clear();
        d.dialog = Some(dlg);
        d.run_ask(text);
        return;
    }
    match outcome {
        Outcome::Open => d.dialog = Some(dlg),
        Outcome::Close if matches!(dlg, Dialog::Star) => {
            crate::views::notices::star_answered(d, false)
        }
        // Escape is Not now too.
        Outcome::Close if matches!(dlg, Dialog::Compression) => {
            crate::views::compression::answer(d, false)
        }
        Outcome::Close => {}
        Outcome::Yes | Outcome::Later => finish(d, ui, dlg, matches!(outcome, Outcome::Yes)),
    }
}

fn heading(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let style = ui.style().with_size(16.).weight(700);
    let ink = ui.theme.ink;
    ui.paragraph(x, y, w, text, style, ink)
}

fn finish(d: &mut Desktop, ui: &mut Ui, dlg: Dialog, yes: bool) {
    match dlg {
        Dialog::Confirm { then, .. } => match then {
            Then::Remove {
                kind,
                id,
                name,
                force,
            } => d.remove(kind, id, name, force),
            Then::Prune(kind) => d.prune(kind),
            Then::Vm(action) => d.run_vm_action(action),
            Then::Docker(to) => d.switch_docker(to),
            Then::Copy(text) => d.copy(&text, ui),
            Then::InstallModule { module, text } => {
                crate::views::training::install(d, module, text)
            }
            Then::RemoveModule(id) => crate::views::training::remove(d, id),
            Then::EndLab => crate::views::training::lab(d, "end", None),
            Then::PracticeRemove { name, session } => {
                crate::views::practice::act(d, "remove", name, session)
            }
            Then::RemoveBuilder {
                session,
                scope,
                name,
            } => {
                if d.session == session {
                    crate::views::builders::act(
                        d,
                        scope,
                        name,
                        podbag_desktop_core::builders::Action::Remove,
                    );
                }
            }
            Then::SyncRemove(name) => {
                crate::views::sync_shares::act(d, podbag_desktop_core::vm::SyncAction::Remove(name))
            }
        },
        Dialog::Star => crate::views::notices::star_answered(d, true),
        Dialog::Compression => crate::views::compression::answer(d, true),
        Dialog::Choose { mut change, .. } => {
            change.restart = yes;
            d.save_vm(change);
        }
        Dialog::Ask { .. }
        | Dialog::KubeObject { .. }
        | Dialog::Migration(_)
        | Dialog::Import(_)
        | Dialog::Run(_) => {}
    }
}
