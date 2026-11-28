//! Offline changes to the local macOS VM data disk.
use podbag_desktop_core::{DiskStorage, VmStorageEdit};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::{button::Kind, text_field::text_field};
use podbag_ui::{Id, TextInput};

use super::blocks::{Buttons, input, spawn};
use super::settings::form_field;
use super::{field, fields};
use crate::{app::Desktop, fmt};

pub const CAPACITY: Id = Id(0x766d640001);
pub const PATH: Id = Id(0x766d640002);
pub const GROW: Id = Id(0x766d640003);
pub const MOVE: Id = Id(0x766d640004);
const RETRY: Id = Id(0x766d640005);
const GIB: u64 = 1 << 30;

pub struct Form {
    disk: Option<DiskStorage>,
    state_at_read: Option<String>,
    busy: bool,
    status: Result<String, String>,
    capacity: TextInput,
    path: TextInput,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            disk: None,
            state_at_read: None,
            busy: false,
            status: Ok(String::new()),
            capacity: input(CAPACITY, ""),
            path: input(PATH, "/Volumes/Work/podbag.img"),
        }
    }
}

impl Form {
    pub fn closed(&mut self) {
        self.state_at_read = None;
    }
}

pub fn shown(d: &Desktop) -> bool {
    d.connection.local
        && d.engine
            .as_ref()
            .is_some_and(|engine| engine.platform == "macos")
}

fn capacity(bytes: u64, lang: &str) -> String {
    format!("{} GiB", fmt::number(bytes as f64 / GIB as f64, 2, lang))
}

pub fn summary(d: &Desktop) -> String {
    d.vm_storage
        .disk
        .as_ref()
        .filter(|disk| disk.exists)
        .map_or_else(String::new, |disk| capacity(disk.size_bytes, d.lang()))
}

fn request(d: &mut Desktop, edit: Option<VmStorageEdit>) {
    let session = d.session;
    let changing = edit.is_some();
    d.vm_storage.busy = true;
    d.vm_storage.state_at_read = Some(d.vm.as_ref().map(|vm| vm.state.clone()).unwrap_or_default());
    spawn(
        d,
        move |api| api.vm_storage(session, edit),
        move |d, result| {
            if d.session != session {
                return;
            }
            let tr = d.tr;
            let f = &mut d.vm_storage;
            f.busy = false;
            match result {
                Ok(disk) => {
                    if changing {
                        f.capacity.set_content("");
                        f.path.set_content("");
                        f.status = Ok(match &disk.backup_path {
                            Some(path) => tr.tv(
                                "settings.vmStorageBackup",
                                &[("path", &path.to_string_lossy())],
                            ),
                            None => tr.t("settings.vmStorageSaved"),
                        });
                    } else {
                        f.status = Ok(String::new());
                    }
                    f.capacity
                        .set_placeholder(&(disk.size_bytes / GIB + 1).to_string());
                    f.disk = Some(disk);
                }
                Err(err) => {
                    if !changing {
                        f.disk = None;
                    }
                    f.status = Err(err);
                }
            }
        },
    );
}

pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let state =
        d.vm.as_ref()
            .map(|vm| vm.state.as_str())
            .unwrap_or_default();
    if !d.vm_storage.busy && d.vm_storage.state_at_read.as_deref() != Some(state) {
        request(d, None);
    }
    let lang = d.lang();
    let tr = d.tr;
    let t = ui.theme;
    let f = &mut d.vm_storage;
    let mut y = y0;
    let mut edit = None;
    if let Some(disk) = &f.disk {
        let mut rows = vec![field(
            tr.t("settings.vmStorageLocation"),
            disk.path.display().to_string(),
            true,
        )];
        if disk.exists {
            rows.push(field(
                tr.t("settings.vmStorageCapacity"),
                capacity(disk.size_bytes, lang),
                false,
            ));
            rows.push(field(
                tr.t("settings.vmStorageAllocated"),
                fmt::size(disk.allocated_bytes, lang),
                false,
            ));
        }
        if let Some(bytes) = disk.filesystem_bytes {
            rows.push(field(
                tr.t("settings.vmStorageFilesystem"),
                capacity(bytes, lang),
                false,
            ));
        }
        if disk.growth_pending {
            rows.push(field(
                tr.t("settings.vmStorageGrowth"),
                tr.t("settings.vmStoragePending"),
                false,
            ));
        }
        y += fields(
            ui,
            Id::of("vm-disk-details"),
            x,
            y,
            w,
            &rows,
            &tr.t("detail.none"),
        ) + 6.;
        let reason = if !disk.editable {
            Some("settings.vmStorageStop")
        } else if !disk.exists {
            Some(if disk.configured {
                "settings.vmStorageMissing"
            } else {
                "settings.vmStorageInitialize"
            })
        } else if disk.filesystem_bytes.is_none() {
            Some("settings.vmStorageUnreadable")
        } else {
            None
        };
        if let Some(key) = reason {
            y += ui.paragraph(x, y, w, &tr.t(key), ui.style(), t.ink2) + 5.;
        } else {
            let rect = form_field(ui, x, &mut y, w, &tr.t("settings.vmStorageNewCapacity"));
            text_field(ui, &mut f.capacity, rect, None, !f.busy);
            let gib = f
                .capacity
                .content()
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|n| *n > disk.size_bytes / GIB && *n <= disk.max_size_gib);
            let mut buttons = Buttons::new(x, y, w);
            if buttons.add(
                ui,
                GROW,
                &tr.t("settings.vmStorageGrow"),
                None,
                Kind::Normal,
                !f.busy && gib.is_some(),
            ) {
                edit = gib.map(|gib| VmStorageEdit::Grow { gib });
            }
            y += buttons.height(y) + 8.;
            let rect = form_field(ui, x, &mut y, w, &tr.t("settings.vmStorageNewPath"));
            text_field(ui, &mut f.path, rect, None, !f.busy);
            let path = f.path.content().trim();
            let mut buttons = Buttons::new(x, y, w);
            if buttons.add(
                ui,
                MOVE,
                &tr.t("settings.vmStorageMove"),
                None,
                Kind::Normal,
                !f.busy && path.starts_with('/') && std::path::Path::new(path) != disk.path,
            ) {
                edit = Some(VmStorageEdit::Move { path: path.into() });
            }
            y += buttons.height(y) + 8.;
        }
    }
    let (message, color) = if f.busy {
        (tr.t("settings.vmStorageWorking"), t.ink2)
    } else {
        match &f.status {
            Ok(msg) => (msg.clone(), t.ink2),
            Err(err) => (err.clone(), t.red),
        }
    };
    if !message.is_empty() {
        y += ui.paragraph(x, y, w, &message, ui.style(), color) + 6.;
    }
    let mut reload = false;
    if !f.busy && f.status.is_err() {
        let mut buttons = Buttons::new(x, y, w);
        reload = buttons.add(ui, RETRY, &tr.t("action.retry"), None, Kind::Normal, true);
        y += buttons.height(y);
    }
    if edit.is_some() || reload {
        request(d, edit);
    }
    y - y0
}
