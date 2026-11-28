//! Inspect and manage the Buildx builders of the selected local Docker endpoint.
use super::{
    blocks::{Buttons, spawn},
    field, fields,
};
use crate::{
    app::Desktop,
    dialogs::{Dialog, Then},
};
use podbag_desktop_core::builders::{Action, Availability, Catalog, Inspection};
use podbag_ui::{Id, ui::Ui, widgets::button::Kind};

pub const REFRESH: Id = Id(0x6278640001);
pub fn row(name: &str) -> Id {
    Id::of(("buildx-row", name))
}
pub fn control(name: &str, key: &str) -> Id {
    Id::of(("buildx-control", name, key))
}

#[derive(Default)]
pub struct Form {
    catalog: Option<Catalog>,
    detail: Option<(String, Inspection)>,
    busy: bool,
    error: String,
}

fn load(d: &mut Desktop, show: Option<String>) {
    let session = d.session;
    d.builders.busy = true;
    spawn(
        d,
        move |api| {
            let catalog = api.builder_list(session)?;
            let selected = show.filter(|name| catalog.builders.iter().any(|b| b.name == *name));
            let detail = match (selected, &catalog.scope) {
                (Some(name), Some(scope)) => {
                    Some((name.clone(), api.builder_inspect(session, scope, &name)?))
                }
                _ => None,
            };
            Ok::<_, String>((catalog, detail))
        },
        move |d, result| {
            if d.session != session {
                return;
            }
            let f = &mut d.builders;
            f.busy = false;
            match result {
                Ok((catalog, detail)) => {
                    f.catalog = Some(catalog);
                    f.detail = detail;
                    f.error.clear();
                }
                Err(err) => {
                    f.detail = None;
                    f.error = err;
                }
            }
        },
    );
}

pub(crate) fn act(d: &mut Desktop, scope: String, name: String, action: Action) {
    if d.builders.busy {
        return;
    }
    let session = d.session;
    d.builders.busy = true;
    let show = (!matches!(action, Action::Remove)).then(|| name.clone());
    spawn(
        d,
        move |api| api.builder_action(session, &scope, &name, action),
        move |d, result| {
            if d.session != session {
                return;
            }
            d.builders.busy = false;
            match result {
                Ok(()) => load(d, show),
                Err(err) => d.builders.error = err,
            }
        },
    );
}

pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    if d.builders.catalog.is_none() && !d.builders.busy && d.builders.error.is_empty() {
        load(d, None);
    }
    let mut y = y0;
    let tr = d.tr;
    let t = ui.theme;
    let f = &d.builders;
    let enabled = !f.busy;
    let mut buttons = Buttons::new(x, y, w);
    let refresh = buttons.add(
        ui,
        REFRESH,
        &tr.t("action.refresh"),
        None,
        Kind::Normal,
        enabled,
    );
    y += buttons.height(y) + 7.;
    let mut show = None;
    let mut clicked = None;
    if let Some(catalog) = &f.catalog {
        let message = match catalog.availability {
            Availability::NonLocal => Some("builders.nonLocal"),
            Availability::MissingDocker => Some("builders.missingDocker"),
            Availability::MissingBuildx => Some("builders.missingBuildx"),
            Availability::Ready if catalog.outside_scope => Some("builders.outsideScope"),
            Availability::Ready if catalog.builders.is_empty() => Some("builders.empty"),
            _ => None,
        };
        if let Some(ctx) = &catalog.context {
            y += ui.paragraph(
                x,
                y,
                w,
                &tr.tv("builders.context", &[("context", ctx)]),
                ui.style(),
                t.ink2,
            ) + 6.;
        }
        if let Some(msg) = message {
            y += ui.paragraph(x, y, w, &tr.t(msg), ui.style(), t.ink2) + 4.;
        }
        for b in &catalog.builders {
            let mut buttons = Buttons::new(x, y, w);
            let mut label = format!("{} · {}", b.name, b.driver);
            if b.current {
                label.push_str(&format!(" · {}", tr.t("builders.current")));
            }
            if buttons.add(ui, row(&b.name), &label, None, Kind::Normal, enabled) {
                show = Some(b.name.clone());
            }
            y += buttons.height(y) + 5.;
            if let Some((name, detail)) = &f.detail
                && name == &b.name
            {
                let mut actions = Buttons::new(x, y, w);
                if actions.add(
                    ui,
                    control(name, "select"),
                    &tr.t("builders.select"),
                    None,
                    Kind::Normal,
                    enabled && !b.current,
                ) {
                    clicked = Some(Action::Select);
                }
                if b.managed {
                    let running = detail.nodes.iter().any(|node| node.status == "running");
                    let (key, action) = if running {
                        ("stop", Action::Stop)
                    } else {
                        ("start", Action::Start)
                    };
                    if actions.add(
                        ui,
                        control(name, key),
                        &tr.t(&format!("action.{key}")),
                        None,
                        Kind::Normal,
                        enabled,
                    ) {
                        clicked = Some(action);
                    }
                    if actions.add(
                        ui,
                        control(name, "remove"),
                        &tr.t("action.remove"),
                        None,
                        Kind::Danger,
                        enabled,
                    ) {
                        clicked = Some(Action::Remove);
                    }
                }
                y += actions.height(y) + 6.;
                if !detail.error.is_empty() {
                    y += ui.paragraph(x, y, w, &detail.error, ui.style(), t.red) + 5.;
                }
                for node in &detail.nodes {
                    y += ui.paragraph(x, y, w, &node.name, ui.style().weight(600), t.ink) + 3.;
                    let status = match node.status.as_str() {
                        "running" | "stopped" | "inactive" | "starting" | "stopping" => {
                            tr.t(&format!("builders.status.{}", node.status))
                        }
                        other => other.into(),
                    };
                    let rows = [
                        ("builders.endpoint", node.endpoint.as_str(), true),
                        ("detail.status", &status, false),
                        ("builders.version", &node.buildkit, true),
                        ("builders.platforms", &node.platforms, false),
                    ]
                    .into_iter()
                    .filter(|(_, value, _)| !value.is_empty())
                    .map(|(key, value, mono)| field(tr.t(key), value, mono))
                    .collect::<Vec<_>>();
                    y += fields(
                        ui,
                        Id::of(("builder-node", name, &node.name)),
                        x,
                        y,
                        w,
                        &rows,
                        &tr.t("detail.none"),
                    ) + 4.;
                    if !node.error.is_empty() {
                        y += ui.paragraph(x, y, w, &node.error, ui.style(), t.red) + 4.;
                    }
                }
            }
        }
    }
    if f.busy {
        y += ui.paragraph(x, y, w, &tr.t("builders.working"), ui.style(), t.ink2) + 4.;
    }
    if !f.error.is_empty() {
        y += ui.paragraph(x, y, w, &f.error, ui.style(), t.red) + 4.;
    }
    let current = f.detail.as_ref().map(|(name, _)| name.clone());
    if refresh {
        load(d, current);
    } else if let Some(name) = show {
        if current.as_ref() == Some(&name) {
            d.builders.detail = None;
        } else {
            load(d, Some(name));
        }
    } else if let (Some(action), Some(name), Some(scope)) = (
        clicked,
        current,
        f.catalog.as_ref().and_then(|c| c.scope.clone()),
    ) {
        if matches!(action, Action::Remove) {
            let text = tr.tv("builders.removeConfirm", &[("name", &name)]);
            d.open_dialog(Dialog::confirm(
                &tr,
                text,
                Some(tr.t("action.remove")),
                Then::RemoveBuilder {
                    session: d.session,
                    scope,
                    name,
                },
            ));
        } else {
            act(d, scope, name, action);
        }
    }
    y - y0
}
