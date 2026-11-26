//! Updates in the settings (app.js `renderUpdates`, `runUpgrade`, `installApp`): the
//! versions, a check, and Update: the terminal's `podbag` first, then the app's package,
//! which starts the new app; this one then ends (`Desktop::quit_soon`).

use podbag_desktop_core::updates::{Check, Updates, Upgraded};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::Kind as ButtonKind;
use podbag_ui::{Color, Id};

use crate::app::Desktop;
use crate::icons::Icon;
use crate::views::blocks::{Buttons, code_box, pre, small, spawn};
use crate::views::{field, fields, notices};

pub const CHECK: Id = Id(0x7570_6461_7401);
pub const INSTALL: Id = Id(0x7570_6461_7402);
pub const OPEN_TERMINAL: Id = Id(0x7570_6461_7403);
pub const APP_NOW: Id = Id(0x7570_6461_7404);
pub const RELEASES: Id = Id(0x7570_6461_7405);

#[derive(Default)]
pub struct UpdateState {
    pub checking: bool,
    pub upgrading: bool,
    /// What the last Update of the terminal's `podbag` did.
    pub upgrade: Option<Upgraded>,
}

/// app.js `loadUpdates`: the last check, and a new one when `check` asks.
pub fn load(d: &mut Desktop, check: Check) {
    if check == Check::Now {
        d.update.checking = true;
    }
    spawn(d, move |api| api.updates(check), loaded);
}

/// The versions came in.
pub fn loaded(d: &mut Desktop, u: Updates) {
    d.updates = Some(u);
    d.update.checking = false;
    notices::terminal_update(d);
}

/// app.js `runUpgrade`: the terminal's `podbag` (to the same version), then the app.
pub fn run_upgrade(d: &mut Desktop) {
    let u = d.updates.clone().unwrap_or_default();
    d.update.upgrading = true;
    d.update.upgrade = None;
    if !u.cli_newer {
        return after_cli(d, &u);
    }
    let version = u.latest.clone();
    spawn(
        d,
        move |api| api.upgrade(version.as_deref()),
        move |d, r| {
            let ok = r.ok;
            d.update.upgrade = Some(r);
            if !ok {
                d.update.upgrading = false;
                return;
            }
            after_cli(d, &u);
        },
    );
}

fn after_cli(d: &mut Desktop, u: &Updates) {
    if u.app_newer && u.installer.is_some() {
        return install_app(d);
    }
    d.update.upgrading = false;
    let text = d.t("update.done");
    d.toasts.push(text, false);
    load(d, Check::Never);
}

/// app.js `installApp`: the app's new package; on success the app ends.
pub fn install_app(d: &mut Desktop) {
    d.update.upgrading = true;
    let latest = d.updates.as_ref().and_then(|u| u.latest.clone());
    spawn(
        d,
        move |api| api.update_app(latest.as_deref()),
        |d, r| match r {
            Ok(()) => {
                let text = d.t("update.restarting");
                d.toasts.push(text, false);
                // The new app (or its installer) starts once this one ends.
                d.quit_soon(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                d.update.upgrading = false;
                d.update.upgrade = Some(Upgraded {
                    ok: false,
                    output: e,
                    ..Upgraded::default()
                });
            }
        },
    );
}

/// The section's value: an available version, or the version in use.
pub fn value(d: &Desktop) -> String {
    let u = d.updates.clone().unwrap_or_default();
    match (&u.latest, u.newer) {
        (Some(latest), true) => d.tv("status.available", &[("version", latest)]),
        _ if u.installer.is_some() => u.app,
        _ => u.engine.unwrap_or_default(),
    }
}

/// The section's body; returns its height.
pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let u = d.updates.clone().unwrap_or_default();
    let (checking, upgrading) = (d.update.checking, d.update.upgrading);
    let mut y = y0;
    let mut rows = vec![
        field(d.t("settings.appVersion"), u.app.clone(), false),
        field(
            d.t("settings.engineVersion"),
            u.engine.clone().unwrap_or_default(),
            false,
        ),
    ];
    if let Some(term) = &u.terminal {
        let v = format!("{} · {term}", u.terminal_version.as_deref().unwrap_or("?"));
        rows.push(field(d.t("settings.terminalVersion"), v, false));
    }
    rows.push(field(
        d.t("settings.latest"),
        u.latest.clone().unwrap_or_default(),
        false,
    ));
    let checked = match u.checked {
        Some(c) if c > 0 => d.ago(c as i64),
        _ => d.t("settings.never"),
    };
    rows.push(field(d.t("settings.checked"), checked, false));
    let none = d.t("detail.none");
    y += fields(ui, Id::of("update-fields"), x, y, w, &rows, &none);
    let status: Option<(String, Color)> = if checking {
        Some((d.t("update.checking"), t.muted))
    } else if let Some(e) = &u.error {
        Some((e.clone(), t.red))
    } else if u.newer {
        let v = u.latest.as_deref().unwrap_or("");
        Some((d.tv("update.available", &[("version", v)]), t.accent_text))
    } else if u.latest.is_some() {
        Some((d.t("update.upToDate"), t.muted))
    } else {
        None
    };
    if let Some((text, color)) = status {
        let style = if u.newer && !checking {
            ui.style().with_size(13.).weight(600)
        } else {
            ui.style().with_size(12.5)
        };
        y += ui.paragraph(x, y + 4., w, &text, style, color) + 12.;
    }
    let mut row = Buttons::new(x, y + 2., w);
    if row.add(
        ui,
        CHECK,
        &d.t("update.check"),
        Some(Icon::Refresh),
        ButtonKind::Normal,
        !checking && !upgrading,
    ) {
        load(d, Check::Now);
    }
    let label = d.t(if upgrading {
        "update.installing"
    } else {
        "update.install"
    });
    if row.add(
        ui,
        INSTALL,
        &label,
        Some(Icon::Update),
        ButtonKind::Primary,
        u.newer && !checking && !upgrading,
    ) {
        run_upgrade(d);
    }
    y += row.height(y + 2.) + 14.;
    y += upgrade_result(d, ui, x, y, w, &u);
    y += app_note(d, ui, x, y, w, &u);
    y - y0
}

/// app.js `upgradeResult`: the command to run as an administrator, or the error.
fn upgrade_result(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, u: &Updates) -> f32 {
    let t = ui.theme;
    let Some(r) = d.update.upgrade.clone().filter(|r| !r.ok) else {
        return 0.;
    };
    let mut y = y0;
    if r.needs_admin {
        let windows = d.engine.as_ref().is_some_and(|e| e.platform == "windows");
        let key = if windows {
            "update.needsAdminWindows"
        } else {
            "update.needsAdmin"
        };
        y += ui.paragraph(x, y, w, &d.t(key), ui.style(), t.ink) + 8.;
        let command = r.command.clone().unwrap_or_default();
        y += code_box(d, ui, Id::of("upgrade-command"), x, y, w, &command) + 10.;
        let mut row = Buttons::new(x, y, w);
        if !windows
            && row.add(
                ui,
                OPEN_TERMINAL,
                &d.t("update.openTerminal"),
                Some(Icon::Shell),
                ButtonKind::Normal,
                true,
            )
            && d.api.open_terminal("upgrade").is_err()
        {
            let text = d.t("terminal.none");
            d.toasts.push(text, true);
        }
        // The terminal's podbag waits for the password; the app can still update now.
        if u.app_newer
            && u.installer.is_some()
            && row.add(
                ui,
                APP_NOW,
                &d.t("update.appNow"),
                Some(Icon::Update),
                ButtonKind::Primary,
                !d.update.upgrading,
            )
        {
            install_app(d);
        }
        y += row.height(y) + 14.;
        return y - y0;
    }
    y += small(ui, x, y, w, &d.t("update.failed"), t.red) + 8.;
    if !r.output.is_empty() {
        y += pre(ui, Id::of("upgrade-output"), x, y, w, &r.output) + 14.;
    }
    y - y0
}

/// app.js `appNote`: how Update treats the app.
fn app_note(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32, u: &Updates) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    if u.installer.is_some() {
        let key = if u.terminal.is_some() {
            "update.appSelfTerminal"
        } else {
            "update.appSelf"
        };
        y += small(ui, x, y, w, &d.t(key), t.muted) + 14.;
        return y - y0;
    }
    y += small(ui, x, y, w, &d.t("update.appManual"), t.muted) + 10.;
    let mut row = Buttons::new(x, y, w);
    if row.add(
        ui,
        RELEASES,
        &d.t("update.openReleases"),
        Some(Icon::Globe),
        ButtonKind::Normal,
        true,
    ) && let Err(e) = d.api.open_releases()
    {
        d.toasts.push(e, true);
    }
    y += row.height(y) + 14.;
    y - y0
}
