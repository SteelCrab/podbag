//! Install local declarative manifests and show their permitted content.
use super::{
    TOOLBAR_H,
    blocks::{Buttons, heading, input, spawn},
    title_bar,
};
use crate::{
    app::Desktop,
    dialogs::{Dialog, Then},
    lists::kind_index,
};
use podbag_desktop_core::{
    Kind,
    extensions::{self, Action, Block, Failure, Installed},
};
use podbag_ui::{
    Id, Rect, TextInput,
    ui::Ui,
    widgets::{
        button::Kind as Button,
        checkbox,
        scroll::{ScrollState, scroll_view},
        text_field::text_field,
    },
};

pub const PATH: Id = Id(0x6578740001);
pub const INSTALL: Id = Id(0x6578740002);
pub const ENABLE: Id = Id(0x6578740003);
pub const GRANT: Id = Id(0x6578740004);
pub const REMOVE: Id = Id(0x6578740005);
pub fn item(id: &str) -> Id {
    Id::of(("extension", id))
}

pub struct PageState {
    items: Vec<Installed>,
    selected: Option<String>,
    busy: bool,
    error: Option<Failure>,
    path: TextInput,
    scroll: ScrollState,
}
impl Default for PageState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: None,
            busy: false,
            error: None,
            path: input(PATH, "manifest.json"),
            scroll: Default::default(),
        }
    }
}

enum Request {
    File(String),
    Action(Action),
}

fn apply(d: &mut Desktop, request: Request) {
    if d.extensions.busy {
        return;
    }
    d.extensions.busy = true;
    d.extensions.error = None;
    let installing = matches!(request, Request::File(_));
    spawn(
        d,
        move |api| {
            let action = match request {
                Request::File(path) => extensions::install_file(std::path::Path::new(&path))?,
                Request::Action(action) => action,
            };
            api.extensions(action)
        },
        move |d, result| {
            let s = &mut d.extensions;
            s.busy = false;
            match result {
                Ok(items) => {
                    if installing {
                        s.selected = items
                            .iter()
                            .find(|new| {
                                !s.items.iter().any(|old| old.manifest.id == new.manifest.id)
                            })
                            .map(|i| i.manifest.id.clone());
                        s.path.set_content("");
                    }
                    if !items
                        .iter()
                        .any(|i| Some(&i.manifest.id) == s.selected.as_ref())
                    {
                        s.selected = items.first().map(|i| i.manifest.id.clone());
                    }
                    s.items = items;
                }
                Err(error) => s.error = Some(error),
            }
        },
    );
}

pub fn refresh(d: &mut Desktop) {
    apply(d, Request::Action(Action::List));
}
pub fn remove(d: &mut Desktop, id: String) {
    apply(d, Request::Action(Action::Remove { id }));
}

pub fn paint(d: &mut Desktop, ui: &mut Ui, mut rect: Rect) {
    let bar = rect.cut_top(TOOLBAR_H);
    title_bar(ui, bar, &d.t("nav.extensions"));
    let mut scroll = std::mem::take(&mut d.extensions.scroll);
    scroll_view(
        ui,
        Id::of("extension-scroll"),
        rect,
        &mut scroll,
        |ui, area| {
            let x = area.x + 20.;
            let w = (area.w - 40.).min(780.);
            let mut y = area.y + 18.;
            let tr = d.tr;
            let t = ui.theme;
            let active = !d.extensions.busy;
            y += ui.paragraph(x, y, w, &tr.t("extensions.manifestPath"), ui.style(), t.ink) + 4.;
            text_field(
                ui,
                &mut d.extensions.path,
                Rect::new(x, y, w.min(520.), 34.),
                None,
                active,
            );
            y += 42.;
            let mut buttons = Buttons::new(x, y, w);
            let path = d.extensions.path.content().trim();
            let install = buttons
                .add(
                    ui,
                    INSTALL,
                    &tr.t("extensions.install"),
                    None,
                    Button::Primary,
                    active && !path.is_empty(),
                )
                .then(|| path.to_owned());
            let reload = buttons.add(
                ui,
                INSTALL.with("refresh"),
                &tr.t("action.refresh"),
                None,
                Button::Normal,
                active,
            );
            y += buttons.height(y) + 14.;
            if let Some(err) = &d.extensions.error {
                y += ui.paragraph(
                    x,
                    y,
                    w,
                    &tr.t(&format!("extensions.error.{}", err.kind)),
                    ui.style(),
                    t.red,
                ) + 4.;
                if let Some(msg) = &err.detail {
                    y += ui.paragraph(x, y, w, msg, ui.style(), t.ink2) + 6.;
                }
            }
            if !active {
                y += ui.paragraph(x, y, w, &tr.t("extensions.loading"), ui.style(), t.muted) + 4.;
            }
            if d.extensions.items.is_empty() {
                if active {
                    y += ui.paragraph(x, y, w, &tr.t("extensions.empty"), ui.style(), t.ink2) + 4.;
                }
            } else {
                y += heading(ui, x, y, w, &tr.t("extensions.installed"), 16.) + 6.;
                let mut rows = Buttons::new(x, y, w);
                for entry in &d.extensions.items {
                    let m = &entry.manifest;
                    let label = format!(
                        "{} · {}",
                        m.name,
                        tr.t(if entry.enabled {
                            "extensions.enabled"
                        } else {
                            "extensions.disabled"
                        })
                    );
                    if rows.add(ui, item(&m.id), &label, None, Button::Normal, active) {
                        d.extensions.selected = Some(m.id.clone());
                    }
                }
                y += rows.height(y) + 15.;
            }
            let mut action = None;
            let mut confirm = None;
            if let Some(entry) = d
                .extensions
                .items
                .iter()
                .find(|e| Some(&e.manifest.id) == d.extensions.selected.as_ref())
            {
                let m = &entry.manifest;
                y += heading(ui, x, y, w, &format!("{} · {}", m.name, m.version), 17.) + 4.;
                y += ui.paragraph(x, y, w, &m.description, ui.style(), t.ink2) + 7.;
                let mut buttons = Buttons::new(x, y, w);
                if buttons.add(
                    ui,
                    ENABLE,
                    &tr.t(if entry.enabled {
                        "extensions.disable"
                    } else {
                        "extensions.enable"
                    }),
                    None,
                    Button::Normal,
                    active,
                ) {
                    action = Some(Action::SetEnabled {
                        id: m.id.clone(),
                        enabled: !entry.enabled,
                    });
                }
                if buttons.add(
                    ui,
                    REMOVE,
                    &tr.t("action.remove"),
                    None,
                    Button::Danger,
                    active,
                ) {
                    confirm = Some((m.id.clone(), m.name.clone()));
                }
                y += buttons.height(y) + 10.;
                if entry.enabled {
                    if m.requests_summary() {
                        let mut granted = entry.engine_summary;
                        let label = tr.t("extensions.permission");
                        if checkbox::checkbox(
                            ui,
                            GRANT,
                            Rect::new(x, y, w, 34.),
                            Some(&mut granted),
                            &label,
                            active,
                        ) {
                            action = Some(Action::SetEngineSummary {
                                id: m.id.clone(),
                                allow: granted,
                            });
                        }
                        y += 40.;
                    }
                    y += extension_view(d, ui, entry, x, y, w);
                }
            }
            if let Some((id, name)) = confirm {
                d.open_dialog(Dialog::confirm(
                    &tr,
                    tr.tv("extensions.confirmRemove", &[("name", &name)]),
                    Some(tr.t("action.remove")),
                    Then::RemoveExtension(id),
                ));
            }
            if let Some(action) = action {
                apply(d, Request::Action(action));
            } else if let Some(path) = install {
                apply(d, Request::File(path));
            } else if reload {
                refresh(d);
            }
            y - area.y + 20.
        },
    );
    d.extensions.scroll = scroll;
}

fn extension_view(d: &Desktop, ui: &mut Ui, entry: &Installed, x: f32, y0: f32, w: f32) -> f32 {
    let mut y = y0 + heading(ui, x, y0, w, &d.t("extensions.view"), 16.) + 5.;
    for block in &entry.manifest.content {
        match block {
            Block::Heading { text } => y += heading(ui, x, y, w, text, 15.) + 5.,
            Block::Text { text } => y += ui.paragraph(x, y, w, text, ui.style(), ui.theme.ink) + 5.,
            Block::Code { text } => {
                y += ui.paragraph(x, y, w, text, ui.style().mono(), ui.theme.ink) + 5.
            }
            Block::List { items } => {
                for text in items {
                    y += ui.paragraph(x, y, w, &format!("• {text}"), ui.style(), ui.theme.ink) + 3.;
                }
            }
            Block::EngineSummary {} => y += engine_summary(d, ui, entry.engine_summary, x, y, w),
        }
    }
    y - y0
}

fn engine_summary(d: &Desktop, ui: &mut Ui, granted: bool, x: f32, y0: f32, w: f32) -> f32 {
    let hint = if !granted {
        Some("extensions.noGrant")
    } else if !d.engine.as_ref().is_some_and(|e| e.reachable) {
        Some("extensions.engineOffline")
    } else {
        None
    };
    if let Some(key) = hint {
        return ui.paragraph(x, y0, w, &d.t(key), ui.style(), ui.theme.ink2) + 4.;
    }
    let mut y = y0 + heading(ui, x, y0, w, &d.t("extensions.engineSummary"), 14.) + 3.;
    for kind in [
        Kind::Containers,
        Kind::Images,
        Kind::Volumes,
        Kind::Networks,
    ] {
        let count = if d.loaded && d.lists.errors[kind_index(kind)].is_none() {
            d.lists.len(kind).to_string()
        } else {
            d.t("detail.none")
        };
        let name = d.t(&format!("nav.{}", kind.segment()));
        y += ui.paragraph(
            x,
            y,
            w,
            &format!("{name}  {count}"),
            ui.style(),
            ui.theme.ink,
        ) + 3.;
    }
    y - y0
}
