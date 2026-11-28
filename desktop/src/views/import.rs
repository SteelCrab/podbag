//! Import from Docker: Docker's named images, to copy
//! the chosen ones to podbag. Docker is only read.

use podbag_desktop_core::images::ImportImage;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind as ButtonKind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::dialog::{buttons_row, dialog};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::{Id, Rect};

use crate::app::Desktop;
use crate::dialogs::{self, Dialog};
use crate::fmt;
use crate::views::blocks::{registry_mark, small, spawn};

pub const OPEN: Id = Id(0x696d_706f_7201);
pub const COPY: Id = Id(0x696d_706f_7202);

/// The checkbox of an image's row.
pub fn row_id(i: usize) -> Id {
    Id::of(("import-row", i))
}

pub struct ImportDialog {
    pub images: Vec<ImportImage>,
    pub checked: Vec<bool>,
    /// podbag has it already.
    pub have: Vec<bool>,
    pub status: String,
    pub working: bool,
    scroll: ScrollState,
}

/// The images toolbar offers the import for podbag when Docker is installed.
pub fn offered(d: &Desktop) -> bool {
    d.is_podbag() && d.docker.as_ref().is_some_and(|c| c.installed)
}

pub fn open(d: &mut Desktop) {
    let hosts = d.pull_hosts.clone();
    spawn(
        d,
        move |api| api.docker_images(&hosts),
        |d, r| match r {
            Err(e) => d.toasts.push(e, true),
            Ok(images) => {
                let have: Vec<bool> = images
                    .iter()
                    .map(|i| d.lists.images.iter().any(|m| m.repo_tags.contains(&i.name)))
                    .collect();
                let n = images.len();
                d.open_dialog(Dialog::Import(Box::new(ImportDialog {
                    images,
                    checked: vec![false; n],
                    have,
                    status: String::new(),
                    working: false,
                    scroll: ScrollState::default(),
                })));
            }
        },
    );
}

fn copy(d: &mut Desktop, m: &mut ImportDialog) {
    let names: Vec<String> = m
        .images
        .iter()
        .zip(&m.checked)
        .filter(|(_, on)| **on)
        .map(|(i, _)| i.name.clone())
        .collect();
    if names.is_empty() {
        return;
    }
    let n = names.len().to_string();
    m.working = true;
    m.status = d.tv("import.working", &[("n", &n)]);
    spawn(
        d,
        move |api| api.import_images(&names),
        move |d, r| {
            let Some(Dialog::Import(m)) = &mut d.dialog else {
                return;
            };
            match r {
                Ok(()) => {
                    d.dialog = None;
                    let text = d.tv("import.done", &[("n", &n)]);
                    d.toasts.push(text, false);
                    d.refresh_soon();
                }
                Err(e) => {
                    m.status = e;
                    m.working = false;
                }
            }
        },
    );
}

/// Paints the dialog; true when it closes.
pub fn paint_dialog(d: &mut Desktop, ui: &mut Ui, m: &mut ImportDialog) -> bool {
    let t = ui.theme;
    let tr = d.tr;
    let lang = d.lang();
    let mut close = false;
    let mut go = false;
    let escape = dialog(ui, dialogs::ID, 520., |ui, r| {
        let mut y = r.y;
        registry_mark(ui, Rect::new(r.x, y + 1., 20., 20.), "dockerhub");
        crate::views::blocks::heading(ui, r.x + 28., y, r.w - 28., &tr.t("import.title"), 16.);
        y += 32.;
        y += ui.paragraph(r.x, y, r.w, &tr.t("import.text"), ui.style(), t.ink2) + 12.;
        if m.images.is_empty() {
            y += ui.paragraph(r.x, y, r.w, &tr.t("import.none"), ui.style(), t.ink) + 12.;
        } else {
            let list_h = (m.images.len() as f32 * 34. + 8.).min(320.);
            let frame = Rect::new(r.x, y, r.w, list_h);
            ui.p.bordered(frame, 8., t.surface, 1., t.line);
            let mut scroll = std::mem::take(&mut m.scroll);
            let have_text = tr.t("import.have");
            scroll_view(
                ui,
                Id::of("import-list"),
                frame.inset(4., 4., 4., 4.),
                &mut scroll,
                |ui, area| {
                    let mut yy = area.y;
                    for i in 0..m.images.len() {
                        let row = Rect::new(area.x, yy, area.w, 34.);
                        yy += 34.;
                        let id = row_id(i);
                        if ui.hovered(id) {
                            ui.p.rounded(row, 6., t.hover);
                        }
                        let image = &m.images[i];
                        let side = if m.have[i] {
                            have_text.clone()
                        } else {
                            fmt::size(image.size.max(0) as u64, lang)
                        };
                        let ss = ui.style().with_size(12.5);
                        let sl = ui.p.layout(&side, ss, None);
                        ui.p.draw_text(
                            &sl,
                            row.right() - 8. - sl.width,
                            row.y + (34. - sl.height) / 2.,
                            t.ink2,
                        );
                        registry_mark(
                            ui,
                            Rect::new(row.x + 32., row.y + 8., 18., 18.),
                            image.registry,
                        );
                        ui.label(
                            Rect::new(row.x + 58., row.y, row.w - 74. - sl.width, 34.),
                            &image.name,
                            ui.style(),
                            t.ink,
                        );
                        ui.tooltip(id.with("tip"), row, &image.name);
                        checkbox::checkbox(
                            ui,
                            id,
                            Rect::new(row.x + 8., row.y, 20., 34.),
                            Some(&mut m.checked[i]),
                            "",
                            !m.working,
                        );
                    }
                    yy - area.y
                },
            );
            m.scroll = scroll;
            y += list_h + 10.;
        }
        if !m.status.is_empty() {
            y += small(ui, r.x, y, r.w, &m.status, t.ink2) + 8.;
        }
        let (cancel, copy_label) = (tr.t("action.cancel"), tr.t("import.copy"));
        let widths = [
            button::width(ui, &copy_label, None, false),
            button::width(ui, &cancel, None, false),
        ];
        let rects = buttons_row(r.x, y + 4., r.w, &widths);
        if button::button(
            ui,
            dialogs::CANCEL,
            rects[1],
            &cancel,
            None,
            ButtonKind::Normal,
            !m.working,
        ) {
            close = true;
        }
        if button::button(
            ui,
            COPY,
            rects[0],
            &copy_label,
            None,
            ButtonKind::Primary,
            !m.images.is_empty() && !m.working,
        ) {
            go = true;
        }
        y + 36. - r.y
    });
    if go {
        copy(d, m);
    }
    (escape && !m.working) || close
}
