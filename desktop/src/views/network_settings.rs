//! Local engine network preferences. A failed save keeps the draft and its revision.
use podbag_desktop_core::{DnsFilter, NetworkPreferences, NetworkSettings};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind};
use podbag_ui::widgets::checkbox;
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{Id, Rect, TextInput};

use crate::app::Desktop;
use crate::views::blocks::{input, spawn};
use crate::views::settings::form_field;

const IPV4: Id = Id(0x6e65740001);
const MODE: Id = Id(0x6e65740002);
const IPV6: Id = Id(0x6e65740003);
const DNS: Id = Id(0x6e65740004);
const FILTER: Id = Id(0x6e65740005);
pub const SAVE: Id = Id(0x6e65740006);
pub const RELOAD: Id = Id(0x6e65740007);

pub const FIELDS: [(&str, Id); 5] = [
    ("networkSettings.ipv4Subnet", IPV4),
    ("networkSettings.ipMode", MODE),
    ("networkSettings.ipv6Subnet", IPV6),
    ("networkSettings.embeddedDns", DNS),
    ("networkSettings.dnsFilter", FILTER),
];

enum Status {
    Idle,
    Loading,
    Saving,
    Saved,
    Error(String),
}

pub struct Form {
    saved: Option<NetworkSettings>,
    status: Status,
    ipv4: TextInput,
    dual: bool,
    ipv6: TextInput,
    embedded_dns: bool,
    dns_filter: DnsFilter,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            saved: None,
            status: Status::Idle,
            ipv4: input(IPV4, "10.77.0.0/16"),
            dual: false,
            ipv6: input(IPV6, "fd77:706f:6462::/64"),
            embedded_dns: false,
            dns_filter: DnsFilter::None,
        }
    }
}

impl Form {
    fn busy(&self) -> bool {
        matches!(self.status, Status::Loading | Status::Saving)
    }

    pub fn enabled(&self, id: Id) -> bool {
        let Some(saved) = &self.saved else {
            return false;
        };
        if self.busy() {
            return false;
        }
        let env = match id {
            IPV4 => "PODBAG_DEFAULT_SUBNET",
            MODE | IPV6 => "PODBAG_DEFAULT_IPV6_SUBNET",
            DNS => "PODBAG_EMBEDDED_DNS",
            FILTER => "PODBAG_DNS_FILTER",
            _ => return false,
        };
        !saved.overrides.iter().any(|name| name == env)
            && (id != IPV6 || self.dual)
            && (id != FILTER || self.embedded_dns)
    }

    fn draft(&self) -> NetworkPreferences {
        NetworkPreferences {
            default_subnet: self.ipv4.content().trim().into(),
            default_ipv6_subnet: self.dual.then(|| self.ipv6.content().trim().into()),
            embedded_dns: self.embedded_dns,
            dns_filter: self.dns_filter,
        }
    }

    fn fill(&mut self, settings: NetworkSettings) {
        let prefs = &settings.preferences;
        self.ipv4.set_content(&prefs.default_subnet);
        self.dual = prefs.default_ipv6_subnet.is_some();
        self.ipv6
            .set_content(prefs.default_ipv6_subnet.as_deref().unwrap_or_default());
        self.embedded_dns = prefs.embedded_dns;
        self.dns_filter = prefs.dns_filter;
        self.saved = Some(settings);
    }
}

fn request(d: &mut Desktop, edit: Option<(NetworkPreferences, String)>) {
    let session = d.session;
    let saving = edit.is_some();
    d.network_settings.status = if saving {
        Status::Saving
    } else {
        Status::Loading
    };
    spawn(
        d,
        move |api| {
            api.network_settings(
                session,
                edit.as_ref()
                    .map(|(values, revision)| (values, revision.as_str())),
            )
        },
        move |d, result| {
            if d.session != session {
                return;
            }
            let form = &mut d.network_settings;
            form.status = match result {
                Ok(settings) => {
                    form.fill(settings);
                    if saving { Status::Saved } else { Status::Idle }
                }
                Err(error) => Status::Error(error),
            };
        },
    );
}

pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    if d.network_settings.saved.is_none() && matches!(d.network_settings.status, Status::Idle) {
        request(d, None);
    }
    let form = &mut d.network_settings;
    let tr = d.tr;
    let t = ui.theme;
    let mut y = y0;
    let mut edit = None;
    if form.saved.is_some() {
        let ipv4 = form_field(ui, x, &mut y, w, &tr.t(FIELDS[0].0));
        let enabled = form.enabled(IPV4);
        text_field(ui, &mut form.ipv4, ipv4, None, enabled);
        let mode = form_field(ui, x, &mut y, w, &tr.t(FIELDS[1].0));
        let ipv4_only = tr.t("networkSettings.ipv4Only");
        let dual_stack = tr.t("networkSettings.dualStack");
        if let Some(i) = select(
            ui,
            MODE,
            mode,
            &[Item::Option(&ipv4_only), Item::Option(&dual_stack)],
            usize::from(form.dual),
            form.enabled(MODE),
        ) {
            form.dual = i == 1;
        }
        if form.dual {
            let ipv6 = form_field(ui, x, &mut y, w, &tr.t(FIELDS[2].0));
            let enabled = form.enabled(IPV6);
            text_field(ui, &mut form.ipv6, ipv6, None, enabled);
        }

        let saved = form.saved.as_ref().unwrap();
        let subnets = [
            saved.active_subnet.as_deref(),
            saved.active_ipv6_subnet.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let active = if subnets.is_empty() {
            tr.t("networkSettings.noActiveBridge")
        } else {
            tr.tv("networkSettings.activeBridge", &[("subnets", &subnets)])
        };
        y += ui.paragraph(x, y, w, &active, ui.style(), t.ink2) + 3.;
        y += ui.paragraph(
            x,
            y,
            w,
            &tr.t("networkSettings.bridgeHint"),
            ui.style(),
            t.ink2,
        ) + 6.;

        let enabled = form.enabled(DNS);
        checkbox::checkbox(
            ui,
            DNS,
            Rect::new(x, y, w, 32.),
            Some(&mut form.embedded_dns),
            &tr.t(FIELDS[3].0),
            enabled,
        );
        y += 36.;
        let filter = form_field(ui, x, &mut y, w, &tr.t(FIELDS[4].0));
        let options = [
            (DnsFilter::None, tr.t("networkSettings.filterNone")),
            (DnsFilter::Ipv4, tr.t("networkSettings.filterA")),
            (DnsFilter::Ipv6, tr.t("networkSettings.filterAAAA")),
        ];
        let items = options.each_ref().map(|(_, label)| Item::Option(label));
        let selected = options
            .iter()
            .position(|(value, _)| *value == form.dns_filter)
            .unwrap_or_default();
        if let Some(i) = select(ui, FILTER, filter, &items, selected, form.enabled(FILTER)) {
            form.dns_filter = options[i].0;
        }
        y += ui.paragraph(
            x,
            y,
            w,
            &tr.t("networkSettings.dnsHint"),
            ui.style(),
            t.ink2,
        ) + 5.;
        if !saved.overrides.is_empty() {
            let text = tr.tv(
                "networkSettings.overridden",
                &[("variables", &saved.overrides.join(", "))],
            );
            y += ui.paragraph(x, y, w, &text, ui.style(), t.ink2) + 6.;
        }
        let values = form.draft();
        let enabled = !form.busy()
            && values != saved.preferences
            && !values.default_subnet.is_empty()
            && values.default_ipv6_subnet.as_deref() != Some("");
        let label = tr.t("action.save");
        let width = button::width(ui, &label, None, false);
        if button::button(
            ui,
            SAVE,
            Rect::new(x, y, width, 32.),
            &label,
            None,
            Kind::Primary,
            enabled,
        ) {
            edit = Some((values, saved.revision.clone()));
        }
        y += 39.;
    }

    let (status, retry) = match &form.status {
        Status::Idle => (String::new(), false),
        Status::Loading => (tr.t("networkSettings.loading"), false),
        Status::Saving => (tr.t("networkSettings.saving"), false),
        Status::Saved => (tr.t("networkSettings.saved"), false),
        Status::Error(error) => (tr.tv("networkSettings.failed", &[("error", error)]), true),
    };
    if !status.is_empty() {
        y += ui.paragraph(
            x,
            y,
            w,
            &status,
            ui.style(),
            if retry { t.red } else { t.ink2 },
        ) + 6.;
    }
    let mut reload = false;
    if retry {
        let label = tr.t(if form.saved.is_some() {
            "networkSettings.reloadSaved"
        } else {
            "action.retry"
        });
        let width = button::width(ui, &label, None, false);
        reload = button::button(
            ui,
            RELOAD,
            Rect::new(x, y, width, 32.),
            &label,
            None,
            Kind::Normal,
            true,
        );
        y += 32.;
    }
    if edit.is_some() || reload {
        request(d, edit);
    }
    y - y0
}
