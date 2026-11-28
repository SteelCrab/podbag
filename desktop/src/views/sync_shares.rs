//! Synchronized shares of the macOS VM, in Settings: a Mac folder kept in sync with a
//! named volume in the VM. The list, Sync now and Stop syncing for each, and a form
//! to add one. Only while the VM runs.

use std::collections::HashSet;

use podbag_desktop_core::vm::{SyncAction, SyncShare};
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::Kind as ButtonKind;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{CursorIcon, Id, Rect, TextInput, TextStyle, icons};

use crate::app::Desktop;
use crate::dialogs::{Dialog, Then};
use crate::icons::Icon;
use crate::views::blocks::{Buttons, input, spawn};

pub const NAME: Id = Id(0x7379_6e63_0001);
pub const PATH: Id = Id(0x7379_6e63_0002);
pub const ADD: Id = Id(0x7379_6e63_0003);
pub const REFRESH: Id = Id(0x7379_6e63_0004);

/// The longest share name.
const MAX_NAME: usize = 64;

/// Sync now of a share.
pub fn now_id(name: &str) -> Id {
    Id::of(("sync-now", name))
}

/// Stop syncing of a share.
pub fn remove_id(name: &str) -> Id {
    Id::of(("sync-remove", name))
}

fn conflicts_id(name: &str) -> Id {
    Id::of(("sync-conflicts", name))
}

pub struct SyncState {
    /// The shares, once read while the VM runs.
    pub shares: Option<Vec<SyncShare>>,
    pub loading: bool,
    pub error: String,
    /// The list was read since the section opened.
    listed: bool,
    pub name: TextInput,
    pub path: TextInput,
    /// The shares whose conflicts show.
    open: HashSet<String>,
}

impl Default for SyncState {
    fn default() -> Self {
        SyncState {
            shares: None,
            loading: false,
            error: String::new(),
            listed: false,
            name: input(NAME, "project"),
            path: input(PATH, "/Users/me/projects/app"),
            open: HashSet::new(),
        }
    }
}

/// Whether the section shows: podbag's VM on a Mac.
pub fn shown(d: &Desktop) -> bool {
    d.has_vm() && d.vm.as_ref().is_some_and(|vm| vm.platform == "macos")
}

fn running(d: &Desktop) -> bool {
    d.vm.as_ref().is_some_and(|vm| vm.state == "running")
}

/// The section closed: read the list again when it opens.
pub fn closed(d: &mut Desktop) {
    d.sync.listed = false;
}

/// Run `action`, then read the list. One at a time, while the VM runs.
pub fn act(d: &mut Desktop, action: SyncAction) {
    if d.sync.loading || !running(d) {
        return;
    }
    let add = matches!(action, SyncAction::Add { .. });
    d.sync.loading = true;
    d.sync.error.clear();
    spawn(
        d,
        move |api| api.vm_sync(&action),
        move |d, result| {
            d.sync.loading = false;
            match result {
                Ok(shares) => {
                    d.sync.shares = Some(shares);
                    if add {
                        d.sync.name.set_content("");
                        d.sync.path.set_content("");
                    }
                }
                Err(e) => d.sync.error = e,
            }
        },
    );
}

fn add(d: &mut Desktop) {
    let (name, path) = (
        d.sync.name.content().trim().to_string(),
        d.sync.path.content().trim().to_string(),
    );
    // Both are needed: the empty one gets the focus.
    if name.is_empty() {
        d.focus_next = Some(NAME);
    } else if path.is_empty() {
        d.focus_next = Some(PATH);
    } else {
        act(d, SyncAction::Add { name, path });
    }
}

/// The section's body; returns its height.
pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    let hint = ui.style().with_size(12.5);
    if !running(d) {
        d.sync.shares = None;
        d.sync.listed = false;
        return ui.paragraph(x, y, w, &d.t("settings.syncStartVm"), hint, t.muted) + 8.;
    }
    if !d.sync.listed && !d.sync.loading {
        d.sync.listed = true;
        act(d, SyncAction::List);
    }
    let loading = d.sync.loading;
    y += ui.paragraph(x, y, w, &d.t("settings.syncHint"), hint, t.muted) + 10.;
    if !d.sync.error.is_empty() {
        let e = d.sync.error.clone();
        y += ui.paragraph(x, y, w, &e, ui.style().with_size(13.), t.red) + 10.;
    }
    // The shares.
    let mono = TextStyle::new(12.5).mono().line_height(18.);
    for share in d.sync.shares.clone().unwrap_or_default() {
        y += ui.paragraph(x, y, w, &share.name, ui.style().weight(600), t.ink) + 2.;
        y += ui.paragraph(x, y, w, &share.source, mono, t.ink2);
        y += ui.paragraph(x, y, w, &share.volume, mono, t.ink2) + 4.;
        if let Some(e) = &share.last_error {
            y += ui.paragraph(x, y, w, e, ui.style().with_size(13.), t.red) + 4.;
        }
        if !share.conflicts.is_empty() {
            let id = conflicts_id(&share.name);
            let open = d.sync.open.contains(&share.name);
            let label = d.tr.tn("settings.syncConflictCount", share.conflicts.len());
            let style = ui.style().with_size(13.);
            let l = ui.p.layout(&label, style, None);
            let r = Rect::new(x, y, l.width + 20., 20.);
            let chevron = if open {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            };
            ui.p.icon(chevron, Rect::new(x, y + 4., 12., 12.), t.muted);
            ui.p.draw_text(&l, x + 18., y + (20. - l.height) / 2., t.ink);
            ui.p.hitbox(r, id, CursorIcon::Pointer);
            ui.focusable(id, FocusKind::Activate, r, 4.);
            if ui.clicked(id) {
                if open {
                    d.sync.open.remove(&share.name);
                } else {
                    d.sync.open.insert(share.name.clone());
                }
            }
            y += 24.;
            if open {
                for c in &share.conflicts {
                    let text = format!("{} – {}", c.path, c.reason);
                    y += ui.paragraph(x + 18., y, w - 18., &text, hint, t.ink2) + 2.;
                }
                y += 4.;
            }
        }
        let mut row = Buttons::new(x, y + 3., w);
        if row.add(
            ui,
            now_id(&share.name),
            &d.t("settings.syncNow"),
            Some(Icon::Refresh),
            ButtonKind::Normal,
            !loading,
        ) {
            act(d, SyncAction::Now(share.name.clone()));
        }
        if row.add(
            ui,
            remove_id(&share.name),
            &d.t("settings.syncRemove"),
            Some(Icon::Remove),
            ButtonKind::Normal,
            !loading,
        ) {
            let text = d.tv("settings.syncRemoveNote", &[("name", &share.name)]);
            let yes = d.t("settings.syncRemove");
            let dialog = Dialog::confirm(&d.tr, text, Some(yes), Then::SyncRemove(share.name));
            d.open_dialog(dialog);
        }
        y += row.height(y + 3.) + 3. + 9.;
        ui.p.fill(Rect::new(x, y, w, 1.), t.line);
        y += 10.;
    }
    // The form: a name and a Mac folder.
    let fw = w.min(480.);
    let label = ui.style().with_size(13.).weight(600);
    for (key, which) in [("settings.syncName", NAME), ("settings.syncPath", PATH)] {
        y += ui.paragraph(x, y, fw, &d.t(key), label, t.ink) + 5.;
        let field = if which == NAME {
            &mut d.sync.name
        } else {
            &mut d.sync.path
        };
        if text_field(ui, field, Rect::new(x, y, fw, 34.), None, true)
            && which == NAME
            && field.content().chars().count() > MAX_NAME
        {
            let kept: String = field.content().chars().take(MAX_NAME).collect();
            field.set_content(&kept);
        }
        if !loading && ui.key_for(which, NamedKey::Enter) {
            add(d);
        }
        y += 34. + 8.;
    }
    let mut row = Buttons::new(x, y + 3., w);
    if row.add(
        ui,
        ADD,
        &d.t("settings.syncAdd"),
        None,
        ButtonKind::Primary,
        !loading,
    ) {
        add(d);
    }
    if row.add(
        ui,
        REFRESH,
        &d.t("action.refresh"),
        Some(Icon::Refresh),
        ButtonKind::Normal,
        !loading,
    ) {
        act(d, SyncAction::List);
    }
    y += row.height(y + 3.) + 3.;
    y - y0
}
