//! Image compression in podbag's store: the question asked once, the switch in the
//! engine's settings, and what the Images view shows of a compressed image.
//!
//! The setting is read from the engine. On a Mac or Windows that runs in the VM, which
//! the command starts when it is stopped, so a stopped VM is not read: the question
//! comes anyway, and the switch waits until the VM runs.

use podbag_desktop_core::compression::{self, Compression, Mode};
use podbag_desktop_core::{Api, CompressionSizes};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::switch::{self, switch};
use podbag_ui::{Id, Rect};

use crate::app::{Desktop, Page};
use crate::dialogs::Dialog;
use crate::fmt;
use crate::icons::Icon;
use crate::views::blocks::spawn;
use crate::views::notices;

pub const SWITCH: Id = Id(0x636f_6d70_0001);

/// What the switch says besides its hint.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Note {
    #[default]
    None,
    /// The VM is off (or its state unknown): the switch waits for it.
    VmOff,
    /// The setting could not be read.
    Error(String),
}

#[derive(Default)]
pub struct CompressionState {
    /// The engine's setting, as last read or set; None while unknown.
    pub current: Option<Compression>,
    pub note: Note,
    /// The setting was read, so the switch can change it.
    pub ready: bool,
    /// A change runs: the value it asked for.
    pub writing: Option<bool>,
    /// Read the setting again when the change ends (Settings opened meanwhile).
    read_after: bool,
    /// Counts the reads; only the result of the last one counts.
    generation: u64,
}

impl CompressionState {
    /// What the switch shows: the value asked for while a change runs.
    pub fn shown_on(&self) -> bool {
        self.writing
            .unwrap_or_else(|| self.current.is_some_and(|c| c.is_on()))
    }

    pub fn enabled(&self) -> bool {
        self.ready && self.writing.is_none()
    }
}

/// How the setting can be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    /// podbag answers without starting anything: Linux, or a running VM.
    Read,
    /// podbag's VM is off, or its state is unknown.
    Stopped,
    /// Another engine: no compression.
    None,
}

fn access(api: &dyn Api) -> Access {
    if !api.connection().local {
        return Access::None;
    }
    match api.vm_status() {
        Ok(None) => Access::Read,
        Ok(Some(vm)) if vm.state == "running" => Access::Read,
        _ => Access::Stopped,
    }
}

/// The question was answered, or compression is already on: it is not asked again.
fn answered(d: &mut Desktop) {
    if !d.settings.compression_prompt_seen {
        d.settings.compression_prompt_seen = true;
        d.save_settings();
    }
}

// ---- the question ----

/// At start, after the star prompt: ask once (on a new install, and after an update to
/// this version), for podbag only, while compression is off. Then the terminal notice.
pub fn prompt(d: &mut Desktop) {
    if d.settings.compression_prompt_seen {
        return notices::terminal_notice(d);
    }
    let session = d.session;
    spawn(
        d,
        |api| {
            let access = access(api);
            let read = (access == Access::Read).then(|| api.compression(None));
            (access, read)
        },
        move |d, (access, read)| {
            if !ask(d, session, access, read) {
                notices::terminal_notice(d);
            }
        },
    );
}

/// Open the question if it is due; true if it opened.
fn ask(
    d: &mut Desktop,
    session: u64,
    access: Access,
    read: Option<Result<Compression, String>>,
) -> bool {
    match (access, read) {
        (Access::None, _) => return false,
        (Access::Read, Some(Ok(state))) => {
            if session == d.session {
                d.compression.current = Some(state);
            }
            if state.is_on() {
                answered(d);
            }
            // A reply without a mode: ask again at the next launch.
            if state.mode != Some(Mode::Off) {
                return false;
            }
        }
        (Access::Read, _) => return false,
        (Access::Stopped, _) => {}
    }
    // Another dialog is open (the user is busy): ask at the next launch.
    if d.in_dialog() {
        return false;
    }
    d.open_dialog(Dialog::Compression);
    true
}

/// The question's answer: Turn on (`on`), or Not now (also Escape).
pub fn answer(d: &mut Desktop, on: bool) {
    if !on {
        answered(d);
        let text = d.t("compression.laterNote");
        d.toasts.push(text, false);
        return notices::terminal_notice(d);
    }
    write(d, true, true);
    notices::terminal_notice(d);
}

// ---- the switch ----

/// Read the setting for the switch. While the VM is off the switch waits, and the read
/// runs again when the VM runs (`vm_changed`).
pub fn read(d: &mut Desktop) {
    if !d.connection.local {
        return;
    }
    // A change finishes first: the read shows its result.
    if d.compression.writing.is_some() {
        d.compression.read_after = true;
        return;
    }
    d.compression.generation += 1;
    let (generation, session) = (d.compression.generation, d.session);
    spawn(
        d,
        |api| {
            let access = access(api);
            let read = (access == Access::Read).then(|| api.compression(None));
            (access, read)
        },
        move |d, (access, read)| {
            let c = &mut d.compression;
            if generation != c.generation || session != d.session {
                return;
            }
            match (access, read) {
                (Access::Read, Some(Ok(state))) => {
                    c.current = Some(state);
                    c.note = Note::None;
                    c.ready = true;
                }
                (Access::Read, Some(Err(e))) => {
                    c.note = Note::Error(e);
                    c.ready = false;
                }
                (Access::Stopped, _) => {
                    c.note = Note::VmOff;
                    c.ready = false;
                }
                _ => {}
            }
        },
    );
}

/// The VM's state was read: a switch that waits for the VM reads the setting once it
/// runs.
pub fn vm_changed(d: &mut Desktop) {
    let running = d.vm.as_ref().is_some_and(|v| v.state == "running");
    if running && d.page == Page::Settings && d.compression.note == Note::VmOff {
        read(d);
    }
}

/// The engine changed: what was read of the old one is gone.
pub fn reset(d: &mut Desktop) {
    let generation = d.compression.generation + 1;
    d.compression = CompressionState {
        generation,
        ..CompressionState::default()
    };
}

/// Turn compression on or off. `asked`: from the question, which says how it went;
/// the switch says only what went wrong.
pub fn write(d: &mut Desktop, on: bool, asked: bool) {
    if d.compression.writing.is_some() {
        return;
    }
    d.compression.writing = Some(on);
    let session = d.session;
    let mode = if on { Mode::Idle } else { Mode::Off };
    spawn(
        d,
        move |api| api.compression(Some(mode)),
        move |d, result| written(d, session, on, asked, result),
    );
}

fn written(
    d: &mut Desktop,
    session: u64,
    on: bool,
    asked: bool,
    result: Result<Compression, String>,
) {
    d.compression.writing = None;
    // An environment setting (PODBAG_IMAGE_COMPRESSION) wins over the saved mode.
    let overridden = d.t("compression.overridden");
    let error = match &result {
        Err(e) if asked => Some(d.tv("compression.failed", &[("error", e)])),
        Err(e) => Some(e.clone()),
        Ok(state) if asked && !state.is_on() => {
            Some(d.tv("compression.failed", &[("error", &overridden)]))
        }
        // A reply without a mode does not say what happened: it fails like an error.
        Ok(state) if state.mode.is_none() => Some(d.t("error.title")),
        Ok(state) if state.is_on() != on => Some(if on {
            d.tv("compression.failed", &[("error", &overridden)])
        } else {
            d.t("compression.offOverridden")
        }),
        Ok(_) => None,
    };
    if let Ok(state) = result
        && state.mode.is_some()
    {
        if session == d.session {
            d.compression.current = Some(state);
        }
        // From the question, only a change that took counts as an answer.
        if !asked || state.is_on() {
            answered(d);
        }
    }
    match error {
        Some(text) => d.toasts.push(text, true),
        None if asked => {
            let text = d.t("compression.done");
            d.toasts.push(text, false);
        }
        None => {}
    }
    if std::mem::take(&mut d.compression.read_after) || (asked && d.page == Page::Settings) {
        read(d);
    }
}

/// The switch's hint: the engine's own age when its setting is known (0: at once),
/// else the default 7 days.
pub fn hint(d: &Desktop) -> String {
    match d.compression.current.and_then(|c| c.after) {
        Some(0) => d.t("compression.hintNow"),
        Some(after) => {
            let time = fmt::age(after, d.lang());
            d.tv("compression.hintAfter", &[("time", &time)])
        }
        None => d.t("compression.hint"),
    }
}

/// The switch in the engine's settings (podbag only), below a line; returns its height.
pub fn setting(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    if !d.connection.local {
        return 0.;
    }
    let t = ui.theme;
    let mut y = y0 + 16.;
    ui.p.fill(Rect::new(x, y, w, 1.), t.line);
    y += 16.;
    let text_w = (w - switch::WIDTH - 16.).max(80.);
    // The label, with the compression icon.
    let label = d.t("compression.label");
    let style = ui.style().weight(600);
    ui.p.icon(
        Icon::Compress.id(),
        Rect::new(x, y + 1., 18., 18.),
        t.accent_text,
    );
    ui.label(
        Rect::new(x + 26., y, text_w - 26., 20.),
        &label,
        style,
        t.ink,
    );
    let mut ty = y + 24.;
    let small = ui.style().with_size(12.5);
    ty += ui.paragraph(x, ty, text_w, &hint(d), small, t.muted);
    // Where to see what it saves, while it is on.
    let c = &d.compression;
    if c.writing.is_none() && c.current.is_some_and(|c| c.is_on()) {
        let note = d.t("compression.imagesNote");
        ty += 4. + ui.paragraph(x, ty + 4., text_w, &note, small, t.muted);
    }
    match d.compression.note.clone() {
        Note::None => {}
        Note::VmOff => {
            let text = d.t("offline.vmTitle");
            ty += 4. + ui.paragraph(x, ty + 4., text_w, &text, small, t.muted);
        }
        Note::Error(e) => ty += 4. + ui.paragraph(x, ty + 4., text_w, &e, small, t.red),
    }
    let mut on = d.compression.shown_on();
    let r = Rect::new(x + w - switch::WIDTH, y, switch::WIDTH, switch::HEIGHT);
    if switch(ui, SWITCH, r, &mut on, d.compression.enabled()) {
        write(d, on, false);
    }
    ui.tooltip(SWITCH, r, &label);
    ty - y0
}

// ---- the Images view ----

/// An image's compressed size on disk, its plain size and the space saved in whole
/// percent, when podbag compressed some of its files.
pub fn of(sizes: Option<&CompressionSizes>) -> Option<(u64, u64, u64)> {
    compression::saved(sizes?)
}

/// The Size cell's second line: "From 8.5 MB, 54% smaller".
pub fn from_text(d: &Desktop, original: u64, saved: u64) -> String {
    let lang = d.lang();
    d.tv(
        "compression.from",
        &[
            ("size", &fmt::size(original, lang)),
            ("percent", &fmt::percent(saved as f64, lang)),
        ],
    )
}

/// The Size cell of a compressed image: the icon and the size on disk, then how much
/// smaller the image got.
pub fn size_cell(d: &Desktop, ui: &mut Ui, cell: Rect, id: Id, sizes: (u64, u64, u64)) {
    let t = ui.theme;
    let (stored, original, saved) = sizes;
    let lang = d.lang();
    let base = ui.style();
    let top = cell.y + (cell.h - 38.) / 2.;
    let icon = Rect::new(cell.x, top + 3., 14., 14.);
    ui.p.icon(Icon::Compress.id(), icon, t.accent_text);
    ui.tooltip(id, icon, &d.t("compression.compressed"));
    let size = fmt::size(stored, lang);
    ui.p.label(
        Rect::new(cell.x + 20., top, cell.w - 20., 20.),
        &size,
        base,
        t.ink,
    );
    let note = from_text(d, original, saved);
    let small = ui.style().with_size(12.).line_height(16.);
    ui.label(
        Rect::new(cell.x, top + 22., cell.w, 16.),
        &note,
        small,
        t.ink2,
    );
}
