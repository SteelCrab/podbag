//! Create and manage the local Kind cluster from Settings.

use podbag_desktop_core::kubernetes::{LocalAction, LocalCluster, Provider};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind};
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{Id, Rect, TextInput};

use crate::app::Desktop;
use crate::dialogs::{Dialog, Then};
use crate::views::blocks::{input, small, spawn};
use crate::views::{field, fields, settings::form_field};

pub const SECTION: &str = "localKubernetes";
pub const OPEN: Id = Id(0x6b75_6265_1000);
pub const PROVIDER: Id = Id(0x6b75_6265_1001);
pub const VERSION: Id = Id(0x6b75_6265_1002);
pub const NODES: Id = Id(0x6b75_6265_1003);
pub const CREATE: Id = Id(0x6b75_6265_1004);
pub const RESET: Id = Id(0x6b75_6265_1005);
pub const REMOVE: Id = Id(0x6b75_6265_1006);
pub const REFRESH: Id = Id(0x6b75_6265_1007);

type Status = Result<Option<LocalCluster>, String>;

pub struct State {
    status: Option<Status>,
    busy: Option<&'static str>,
    error: Option<String>,
    provider: usize,
    version: TextInput,
    nodes: TextInput,
}

impl Default for State {
    fn default() -> Self {
        let mut nodes = input(NODES, "");
        nodes.set_content("1");
        Self {
            status: None,
            busy: None,
            error: None,
            provider: 0,
            version: input(VERSION, ""),
            nodes,
        }
    }
}

impl State {
    pub fn cluster(&self) -> Option<&LocalCluster> {
        self.status.as_ref()?.as_ref().ok()?.as_ref()
    }
}

fn ready_key(cluster: &LocalCluster) -> &'static str {
    if cluster.ready {
        "kube.localProvisioned"
    } else {
        "kube.localIncomplete"
    }
}

pub fn summary(d: &Desktop) -> String {
    d.local_kube
        .cluster()
        .map_or_else(String::new, |c| d.t(ready_key(c)))
}

pub fn refresh(d: &mut Desktop) {
    if d.local_kube.busy.is_some() {
        return;
    }
    d.local_kube.busy = Some("engine.checking");
    d.local_kube.error = None;
    spawn(
        d,
        |api| api.local_kube_status(),
        |d, status| {
            d.local_kube.status = Some(status);
            d.local_kube.busy = None;
        },
    );
}

pub fn change(d: &mut Desktop, action: LocalAction) {
    if d.local_kube.busy.is_some() {
        return;
    }
    let (progress, removed) = match &action {
        LocalAction::Create { .. } => ("kube.localCreating", None),
        LocalAction::Reset(_) => ("kube.localResetting", None),
        LocalAction::Remove(context) => ("kube.localRemoving", Some(context.clone())),
    };
    let creating = matches!(action, LocalAction::Create { .. });
    d.local_kube.busy = Some(progress);
    d.local_kube.error = None;
    spawn(
        d,
        move |api| match api.local_kube_change(action) {
            Ok(cluster) => (Ok(cluster), None),
            Err(e) => (api.local_kube_status(), Some(e)),
        },
        move |d, (status, error)| {
            d.local_kube.busy = None;
            if error.is_none() {
                if creating {
                    d.kube.context = status
                        .as_ref()
                        .ok()
                        .and_then(|c| c.as_ref())
                        .map(|c| c.context.clone());
                } else if removed.is_some() && d.kube.context == removed {
                    d.kube.context = None;
                }
                d.kube_reload();
            }
            d.local_kube.status = Some(status);
            d.local_kube.error = error;
        },
    );
}

pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    if d.local_kube.status.is_none() && d.local_kube.busy.is_none() {
        refresh(d);
    }
    let platform = d.engine.as_ref().map(|e| e.platform);
    let mac = platform.map_or(cfg!(target_os = "macos"), |p| p == "macos");
    let linux = platform.map_or(cfg!(target_os = "linux"), |p| p == "linux");
    let prerequisites = if mac {
        "kube.localPrerequisitesMac"
    } else if linux {
        "kube.localPrerequisites"
    } else {
        "kube.localPrerequisitesDocker"
    };
    y += small(ui, x, y, w, &d.t(prerequisites), t.ink2) + 12.;
    let enabled = d.local_kube.busy.is_none();
    if let Some(cluster) = d.local_kube.cluster().cloned() {
        let engine = match cluster.provider {
            Provider::Docker => d.t("kube.localDockerEngine"),
            Provider::Podbag => "Podbag".into(),
            Provider::PodbagVm => d.t("kube.localPodbagVm"),
        };
        let rows = [
            field(d.t("kube.cluster"), cluster.context.clone(), true),
            field(d.t("settings.engine"), engine, false),
            field(
                d.t("kube.localVersion"),
                cluster
                    .version
                    .clone()
                    .unwrap_or_else(|| d.t("kube.localDefault")),
                false,
            ),
            field(d.t("kube.localNodes"), cluster.nodes.to_string(), false),
            field(d.t("settings.vmState"), d.t(ready_key(&cluster)), false),
        ];
        y += fields(ui, OPEN.with("summary"), x, y, w, &rows, "") + 12.;
        let reset = d.t("kube.localReset");
        let rw = button::width(ui, &reset, None, false);
        if button::button(
            ui,
            RESET,
            Rect::new(x, y, rw, 34.),
            &reset,
            None,
            Kind::Normal,
            enabled,
        ) {
            d.dialog = Some(Dialog::confirm(
                &d.tr,
                d.t("kube.localResetConfirm"),
                Some(reset),
                Then::LocalCluster(LocalAction::Reset(cluster.context.clone())),
            ));
        }
        let remove = d.t("action.remove");
        let bw = button::width(ui, &remove, None, false);
        if button::button(
            ui,
            REMOVE,
            Rect::new(x + rw + 8., y, bw, 34.),
            &remove,
            None,
            Kind::Danger,
            enabled,
        ) {
            d.dialog = Some(Dialog::confirm(
                &d.tr,
                d.t("kube.localRemoveConfirm"),
                Some(remove),
                Then::LocalCluster(LocalAction::Remove(cluster.context)),
            ));
        }
        y += 42.;
    } else if matches!(d.local_kube.status, Some(Ok(None))) {
        if mac {
            let options = [d.t("kube.localDockerEngine"), d.t("kube.localPodbagVm")];
            let items = options.each_ref().map(|s| Item::Option(s.as_str()));
            let rect = form_field(ui, x, &mut y, w, &d.t("settings.engine"));
            if let Some(next) = select(ui, PROVIDER, rect, &items, d.local_kube.provider, enabled) {
                d.local_kube.provider = next;
            }
        }
        let rect = form_field(ui, x, &mut y, w, &d.t("kube.localVersion"));
        let placeholder = d.t("kube.localDefault");
        d.local_kube.version.set_placeholder(&placeholder);
        text_field(ui, &mut d.local_kube.version, rect, None, enabled);
        let nodes_label = format!("{} (1–255)", d.t("kube.localNodes"));
        let rect = form_field(ui, x, &mut y, w, &nodes_label);
        text_field(ui, &mut d.local_kube.nodes, rect, None, enabled);
        let nodes = d
            .local_kube
            .nodes
            .content()
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|&n| n > 0);
        let label = d.t("kube.localCreate");
        let bw = button::width(ui, &label, None, false);
        if button::button(
            ui,
            CREATE,
            Rect::new(x, y, bw, 34.),
            &label,
            None,
            Kind::Primary,
            enabled && nodes.is_some(),
        ) && let Some(nodes) = nodes
        {
            let version = d.local_kube.version.content().trim();
            let version = (!version.is_empty()).then(|| version.to_owned());
            let managed_vm = mac && d.local_kube.provider == 1;
            change(
                d,
                LocalAction::Create {
                    version,
                    nodes,
                    managed_vm,
                },
            );
        }
        y += 42.;
    }
    if let Some(progress) = d.local_kube.busy {
        y += small(ui, x, y, w, &d.t(progress), t.ink2);
    } else if let Some(error) = d.local_kube.error.as_ref().or_else(|| {
        d.local_kube
            .status
            .as_ref()
            .and_then(|status| status.as_ref().err())
    }) {
        y += small(ui, x, y, w, error, t.red) + 10.;
        let label = d.t("action.refresh");
        let bw = button::width(ui, &label, None, false);
        if button::button(
            ui,
            REFRESH,
            Rect::new(x, y, bw, 34.),
            &label,
            None,
            Kind::Normal,
            true,
        ) {
            refresh(d);
        }
        y += 42.;
    }
    y - y0
}
