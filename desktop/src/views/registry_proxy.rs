//! The local engine's proxy for image registry requests.
use podbag_desktop_core::{RegistryProxyMode as Mode, RegistryProxySettings as Settings};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind};
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{Id, Rect, TextInput};

use crate::app::Desktop;
use crate::views::blocks::{input, spawn};
use crate::views::settings::form_field;

pub const MODE: Id = Id(0x70726f780001);
pub const URL: Id = Id(0x70726f780002);
pub const BYPASS: Id = Id(0x70726f780003);
pub const SAVE: Id = Id(0x70726f780004);
const RETRY: Id = Id(0x70726f780005);

const MODES: [(Mode, &str); 3] = [
    (Mode::Inherited, "settings.registryProxyInherited"),
    (Mode::Direct, "settings.registryProxyDirect"),
    (Mode::Manual, "settings.registryProxyManual"),
];

#[derive(PartialEq, Eq)]
enum Status {
    Idle,
    Working,
    Saved,
    LoadError,
    SaveError,
}

pub struct Form {
    saved: Option<Settings>,
    status: Status,
    mode: Mode,
    url: TextInput,
    bypass: TextInput,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            saved: None,
            status: Status::Idle,
            mode: Mode::Inherited,
            url: input(URL, "http://proxy.example.com:3128"),
            bypass: input(BYPASS, "localhost, .example.com"),
        }
    }
}

impl Form {
    fn enabled(&self, id: Id) -> bool {
        self.saved.is_some()
            && self.status != Status::Working
            && (id == MODE || self.mode == Mode::Manual && matches!(id, URL | BYPASS))
    }

    fn draft(&self) -> Settings {
        let mut values = Settings {
            mode: self.mode,
            ..Settings::default()
        };
        if values.mode == Mode::Manual {
            values.url = self.url.content().trim().into();
            values.bypass = self.bypass.content().into();
        }
        values
    }

    fn fill(&mut self, values: Settings) {
        self.mode = values.mode;
        self.url.set_content(&values.url);
        self.bypass.set_content(&values.bypass);
        self.saved = Some(values);
    }
}

fn mode_index(mode: Mode) -> usize {
    match mode {
        Mode::Inherited => 0,
        Mode::Direct => 1,
        Mode::Manual => 2,
    }
}

pub fn shown(d: &Desktop) -> bool {
    d.connection.local
        && d.engine
            .as_ref()
            .is_some_and(|e| matches!(e.platform, "linux" | "macos"))
}

pub fn summary(d: &Desktop) -> String {
    d.registry_proxy
        .saved
        .as_ref()
        .map_or_else(String::new, |settings| {
            d.t(MODES[mode_index(settings.mode)].1)
        })
}

pub fn closed(d: &mut Desktop) {
    if d.registry_proxy.status == Status::LoadError {
        d.registry_proxy.status = Status::Idle;
    }
}

fn request(d: &mut Desktop, edit: Option<Settings>) {
    let session = d.session;
    let saving = edit.is_some();
    d.registry_proxy.status = Status::Working;
    spawn(
        d,
        move |api| api.registry_proxy(session, edit),
        move |d, result| {
            if d.session != session {
                return;
            }
            let form = &mut d.registry_proxy;
            form.status = match result {
                Ok(values) => {
                    form.fill(values);
                    if saving { Status::Saved } else { Status::Idle }
                }
                Err(_) if saving => Status::SaveError,
                Err(_) => Status::LoadError,
            };
        },
    );
}

pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    if d.registry_proxy.saved.is_none() && d.registry_proxy.status == Status::Idle {
        request(d, None);
    }
    let form = &mut d.registry_proxy;
    let tr = d.tr;
    let t = ui.theme;
    let mut y = y0;
    y += ui.paragraph(
        x,
        y,
        w,
        &tr.t("settings.registryProxyDescription"),
        ui.style(),
        t.ink2,
    ) + 10.;

    let rect = form_field(ui, x, &mut y, w, &tr.t("settings.registryProxyMode"));
    let labels = MODES.map(|(_, key)| tr.t(key));
    let items = labels.each_ref().map(|label| Item::Option(label));
    if let Some(i) = select(
        ui,
        MODE,
        rect,
        &items,
        mode_index(form.mode),
        form.enabled(MODE),
    ) {
        form.mode = MODES[i].0;
    }
    if form.mode == Mode::Manual {
        let rect = form_field(ui, x, &mut y, w, &tr.t("settings.registryProxyUrl"));
        let enabled = form.enabled(URL);
        text_field(ui, &mut form.url, rect, None, enabled);
        y += ui.paragraph(
            x,
            y,
            w,
            &tr.t("settings.registryProxyUrlHint"),
            ui.style(),
            t.ink2,
        ) + 6.;

        let rect = form_field(ui, x, &mut y, w, &tr.t("settings.registryProxyBypass"));
        let enabled = form.enabled(BYPASS);
        text_field(ui, &mut form.bypass, rect, None, enabled);
        y += ui.paragraph(
            x,
            y,
            w,
            &tr.t("settings.registryProxyBypassHint"),
            ui.style(),
            t.ink2,
        ) + 6.;
    }

    let values = form.draft();
    let enabled = form.enabled(MODE)
        && form.saved.as_ref() != Some(&values)
        && (values.mode != Mode::Manual || !values.url.is_empty());
    let label = tr.t("action.save");
    let width = button::width(ui, &label, None, false);
    let save = button::button(
        ui,
        SAVE,
        Rect::new(x, y, width, 32.),
        &label,
        None,
        Kind::Primary,
        enabled,
    );
    y += 39.;

    let message = match form.status {
        Status::Idle | Status::Working => None,
        Status::Saved => Some(("settings.registryProxySaved", t.ink2)),
        Status::LoadError => Some(("settings.registryProxyLoadError", t.red)),
        Status::SaveError => Some(("settings.registryProxySaveError", t.red)),
    };
    if let Some((key, color)) = message {
        y += ui.paragraph(x, y, w, &tr.t(key), ui.style(), color) + 6.;
    }

    if form.status == Status::LoadError {
        let label = tr.t("action.retry");
        let width = button::width(ui, &label, None, false);
        if button::button(
            ui,
            RETRY,
            Rect::new(x, y, width, 32.),
            &label,
            None,
            Kind::Normal,
            true,
        ) {
            request(d, None);
        }
        y += 32.;
    } else if save {
        request(d, Some(values));
    }
    y - y0
}
