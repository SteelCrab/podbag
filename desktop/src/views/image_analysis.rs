//! Review the selected engine's image with a local Trivy scan.

use std::sync::Arc;

use podbag_desktop_core::Kind;
use podbag_desktop_core::image_analysis::{Failure, Report, Severity};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, Kind as ButtonKind};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::{Color, Id, Rect, TextStyle};

use crate::app::{Desktop, Tab};
use crate::fmt;
use crate::icons::Icon;
use crate::tr::Tr;
use crate::views::blocks::spawn;

pub const SCAN: Id = Id(0x0061_6e61_6c79_7301);
const FINDINGS_MORE: Id = Id(0x0061_6e61_6c79_7302);
const PACKAGES_MORE: Id = Id(0x0061_6e61_6c79_7303);
const PAGE: usize = 50;
const ROW: f32 = 29.;

#[derive(Default)]
pub struct State {
    active: Option<Arc<()>>,
    result: Option<Result<Report, Failure>>,
    findings_more: usize,
    packages_more: usize,
    scroll: ScrollState,
}

pub fn scan(d: &mut Desktop) {
    let Some(detail) = d
        .detail
        .as_mut()
        .filter(|detail| detail.kind == Kind::Images && detail.tab == Tab::Analysis)
    else {
        return;
    };
    if detail.analysis.active.is_some() {
        return;
    }
    let session = d.session;
    let id = detail.id.clone();
    let request = Arc::new(());
    detail.analysis = State {
        active: Some(request.clone()),
        ..State::default()
    };
    spawn(
        d,
        move |api| api.analyze_image(session, &id),
        move |d, result| {
            if d.session != session {
                return;
            }
            let Some(detail) = &mut d.detail else { return };
            let state = &mut detail.analysis;
            // Closing and reopening the same image gives it a different request.
            if state
                .active
                .as_ref()
                .is_none_or(|active| !Arc::ptr_eq(active, &request))
            {
                return;
            }
            state.active = None;
            state.result = Some(result.and_then(|report| {
                if report.image_id == detail.id {
                    Ok(report)
                } else {
                    Err(Failure::ImageUnavailable)
                }
            }));
        },
    );
}

pub fn paint(d: &mut Desktop, ui: &mut Ui, panel: Rect) {
    let tr = d.tr;
    let Some(detail) = &mut d.detail else { return };
    let state = &mut detail.analysis;
    let t = ui.theme;
    let scanning = state.active.is_some();
    let scan_text = tr.t(if scanning {
        "analysis.scanning"
    } else {
        "analysis.scan"
    });
    let w = button::width(ui, &scan_text, Some(Icon::Search.id()), false);
    let scan_r = Rect::new(panel.right() - w, panel.y, w, 32.);
    let run = button::button(
        ui,
        SCAN,
        scan_r,
        &scan_text,
        Some(Icon::Search.id()),
        ButtonKind::Primary,
        !scanning,
    );
    let hint_h = ui.paragraph(
        panel.x,
        panel.y + 5.,
        (panel.w - w - 12.).max(16.),
        &tr.t("analysis.hint"),
        ui.style(),
        t.ink2,
    ) + 5.;
    let mut report_area = panel;
    report_area.cut_top(hint_h.max(32.) + 12.);
    if let Some(result) = &state.result {
        match result {
            Err(error) => {
                ui.paragraph(
                    report_area.x,
                    report_area.y,
                    report_area.w,
                    &tr.t(error.key()),
                    ui.style(),
                    t.red,
                );
            }
            Ok(report) => {
                scroll_view(
                    ui,
                    Id::of("image-analysis-report"),
                    report_area,
                    &mut state.scroll,
                    |ui, area| {
                        report_panel(
                            ui,
                            area,
                            tr,
                            report,
                            &mut state.findings_more,
                            &mut state.packages_more,
                        )
                    },
                );
            }
        }
    }
    if run {
        scan(d);
    }
}

fn title(ui: &mut Ui, area: Rect, y: f32, text: &str) -> f32 {
    let style = ui.style().with_size(16.).weight(700);
    ui.paragraph(area.x, y, area.w, text, style, ui.theme.ink) + 8.
}

fn report_panel(
    ui: &mut Ui,
    area: Rect,
    tr: Tr,
    report: &Report,
    findings_more: &mut usize,
    packages_more: &mut usize,
) -> f32 {
    let none = tr.t("detail.none");
    let find_columns = [
        ("analysis.severity", 0.13),
        ("analysis.advisory", 0.24),
        ("analysis.package", 0.17),
        ("analysis.installed", 0.15),
        ("analysis.fixed", 0.15),
        ("analysis.target", 0.16),
    ];
    let pkg_columns = [
        ("analysis.package", 0.42),
        ("settings.version", 0.27),
        ("analysis.target", 0.31),
    ];
    let t = ui.theme;
    let style = ui.style();
    let mut y = area.y + 4.;
    let count_title = |key, count| tr.tv(key, &[("n", &fmt::number(count as f64, 0, tr.lang))]);
    y += title(
        ui,
        area,
        y,
        &count_title("analysis.findings", report.findings.len()),
    );
    y += summary(ui, area, y, tr, report);
    if report.findings.is_empty() {
        y += ui.paragraph(
            area.x,
            y,
            area.w,
            &tr.t("analysis.noFindings"),
            ui.style(),
            t.ink2,
        );
    } else {
        y += header(ui, area, y, tr, &find_columns);
        let limit = PAGE.saturating_mul(findings_more.saturating_add(1));
        for row in report.findings.iter().take(limit) {
            let level = tr.t(row.severity.key());
            let text = [
                level.as_str(),
                &row.advisory,
                &row.package,
                &row.installed,
                if row.fixed.is_empty() {
                    &none
                } else {
                    &row.fixed
                },
                &row.target,
            ];
            let (ink, _) = severity_colors(ui, row.severity);
            y += cells(ui, area, y, &find_columns, &text, style, ink);
        }
        y += more(
            ui,
            area,
            y,
            tr,
            FINDINGS_MORE,
            report.findings.len() > limit,
            findings_more,
        );
    }
    y += 22.;
    y += title(
        ui,
        area,
        y,
        &count_title("analysis.packages", report.packages.len()),
    );
    if report.packages.is_empty() {
        y += ui.paragraph(
            area.x,
            y,
            area.w,
            &tr.t("analysis.noPackages"),
            ui.style(),
            t.ink2,
        );
    } else {
        y += header(ui, area, y, tr, &pkg_columns);
        let limit = PAGE.saturating_mul(packages_more.saturating_add(1));
        for pkg in report.packages.iter().take(limit) {
            let version = if pkg.version.is_empty() {
                &none
            } else {
                &pkg.version
            };
            y += cells(
                ui,
                area,
                y,
                &pkg_columns,
                &[&pkg.name, version, &pkg.target],
                style,
                t.ink,
            );
        }
        y += more(
            ui,
            area,
            y,
            tr,
            PACKAGES_MORE,
            report.packages.len() > limit,
            packages_more,
        );
    }
    y - area.y + 16.
}

fn severity_colors(ui: &Ui, severity: Severity) -> (Color, Color) {
    let t = ui.theme;
    match severity {
        Severity::Critical | Severity::High => (t.red, t.red_soft),
        Severity::Medium => (t.amber, t.amber_soft),
        Severity::Low => (t.accent_text, t.accent_soft),
        Severity::Unknown => (t.ink2, t.gray_soft),
    }
}

fn summary(ui: &mut Ui, area: Rect, y: f32, tr: Tr, report: &Report) -> f32 {
    let counts = report.findings.iter().fold([0; 5], |mut counts, finding| {
        counts[finding.severity as usize] += 1;
        counts
    });
    let w = (area.w - 32.) / 5.;
    for (i, (severity, count)) in Severity::ALL.into_iter().zip(counts).enumerate() {
        let x = area.x + i as f32 * (w + 8.);
        let (fg, bg) = severity_colors(ui, severity);
        ui.p.rounded(Rect::new(x, y, w, 51.), 7., bg);
        ui.label(
            Rect::new(x + 8., y + 5., (w - 16.).max(8.), 17.),
            &tr.t(severity.key()),
            ui.style().with_size(12.),
            fg,
        );
        ui.label(
            Rect::new(x + 8., y + 24., (w - 16.).max(8.), 22.),
            &fmt::number(f64::from(count), 0, tr.lang),
            ui.style().weight(700),
            fg,
        );
    }
    62.
}

fn header(ui: &mut Ui, area: Rect, y: f32, tr: Tr, cols: &[(&str, f32)]) -> f32 {
    ui.p.fill(Rect::new(area.x, y, area.w, ROW), ui.theme.sunken);
    let labels: Vec<_> = cols.iter().map(|(key, _)| tr.t(key)).collect();
    let text: Vec<_> = labels.iter().map(String::as_str).collect();
    let style = ui.style().weight(600);
    let ink = ui.theme.ink2;
    cells(ui, area, y, cols, &text, style, ink)
}

fn cells(
    ui: &mut Ui,
    area: Rect,
    y: f32,
    cols: &[(&str, f32)],
    text: &[&str],
    style: TextStyle,
    first_ink: Color,
) -> f32 {
    let mut x = area.x;
    for (i, ((_, weight), value)) in cols.iter().zip(text).enumerate() {
        let w = area.w * weight;
        let ink = if i == 0 { first_ink } else { ui.theme.ink };
        let rect = Rect::new(x + 8., y, (w - 16.).max(8.), ROW);
        ui.label(rect, value, style, ink);
        ui.tooltip(Id::of(("image-analysis-cell", y.to_bits(), i)), rect, value);
        x += w;
    }
    ui.p.border_bottom(Rect::new(area.x, y, area.w, ROW), ui.theme.line);
    ROW
}

fn more(ui: &mut Ui, area: Rect, y: f32, tr: Tr, id: Id, has_more: bool, page: &mut usize) -> f32 {
    if !has_more {
        return 0.;
    }
    let label = tr.t("analysis.more");
    let w = button::width(ui, &label, None, false);
    if button::button(
        ui,
        id,
        Rect::new(area.x, y + 8., w, 30.),
        &label,
        None,
        ButtonKind::Normal,
        true,
    ) {
        *page += 1;
    }
    38.
}
