//! Push buttons and icon buttons. Both take the focus (Tab), and Enter or Space click them.

use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::{Color, Rect};
use crate::scene::Id;
use crate::ui::Ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Normal,
    Primary,
    Danger,
}

const PAD: f32 = 12.;
const ICON: f32 = 16.;
const GAP: f32 = 6.;

/// Width of a button with this label and icon.
pub fn width(ui: &mut Ui, label: &str, icon: Option<u16>, small: bool) -> f32 {
    let style = ui
        .style()
        .weight(500)
        .with_size(if small { 12.5 } else { ui.theme.text_size });
    let text = ui.text(label, style);
    let pad = if small { 9. } else { PAD };
    let icon_w = if icon.is_some() {
        ICON + if label.is_empty() { 0. } else { GAP }
    } else {
        0.
    };
    (pad * 2. + icon_w + text.width).ceil() + 2.
}

/// A button filling `rect`. Returns true when clicked.
pub fn button(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    label: &str,
    icon: Option<u16>,
    kind: Kind,
    enabled: bool,
) -> bool {
    let t = ui.theme;
    let hovered = enabled && ui.hovered(id);
    let (mut bg, mut border, mut fg) = match kind {
        Kind::Normal => (if hovered { t.hover } else { t.surface }, t.line, t.ink),
        Kind::Primary => (t.accent, t.accent, t.accent_ink),
        Kind::Danger => (
            t.danger_bg,
            t.danger_bg,
            Color {
                r: 1.,
                g: 1.,
                b: 1.,
                a: 1.,
            },
        ),
    };
    if hovered && kind != Kind::Normal {
        bg = brighten(bg);
    }
    if !enabled {
        for c in [&mut bg, &mut border, &mut fg] {
            *c = c.with_alpha(0.45);
        }
    }
    ui.p.bordered(rect, 7., bg, 1., border);
    let small = rect.h < 30.;
    let style = ui
        .style()
        .weight(500)
        .with_size(if small { 12.5 } else { t.text_size });
    let text = ui.text(label, style);
    let icon_w = if icon.is_some() {
        ICON + if label.is_empty() { 0. } else { GAP }
    } else {
        0.
    };
    let content = icon_w + text.width;
    let mut x = rect.x + ((rect.w - content) / 2.).max(PAD.min(rect.w / 4.));
    if let Some(icon) = icon {
        ui.p.icon(
            icon,
            Rect::new(x, rect.y + (rect.h - ICON) / 2., ICON, ICON),
            fg,
        );
        x += icon_w;
    }
    let inner = Rect::new(x, rect.y, (rect.right() - x - 4.).max(0.), rect.h);
    ui.label(inner, label, style, fg);
    if !enabled {
        return false;
    }
    ui.p.hitbox(rect, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, rect, 7.);
    ui.clicked(id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconKind {
    Plain,
    Accent,
    Danger,
}

/// An icon-only button (30 x 30 usually; pass a smaller rect for compact rows).
/// `label` names it and shows as its tooltip. Returns true when clicked.
pub fn icon_button(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    icon: u16,
    label: &str,
    kind: IconKind,
    enabled: bool,
) -> bool {
    let t = ui.theme;
    let hovered = enabled && ui.hovered(id);
    let mut fg = match kind {
        IconKind::Accent => t.accent_text,
        _ => t.ink2,
    };
    if hovered {
        match kind {
            IconKind::Danger => {
                ui.p.rounded(rect, 7., t.red_soft);
                fg = t.red;
            }
            _ => {
                ui.p.bordered(rect, 7., t.hover, 1., t.line);
                if kind == IconKind::Plain {
                    fg = t.ink;
                }
            }
        }
    }
    if !enabled {
        fg = fg.with_alpha(0.45);
    }
    let s = (rect.h * 0.57).round();
    ui.p.icon(icon, rect.center(s, s), fg);
    if !enabled {
        return false;
    }
    ui.p.hitbox(rect, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, rect, 7.);
    ui.tooltip(id, rect, label);
    ui.clicked(id)
}

fn brighten(c: Color) -> Color {
    let f = |v: f32| (v * 1.06).min(1.);
    Color {
        r: f(c.r),
        g: f(c.g),
        b: f(c.b),
        a: c.a,
    }
}
