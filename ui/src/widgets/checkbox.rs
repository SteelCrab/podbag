//! A checkbox with a label. Space or Enter toggles it when focused.

use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::Rect;
use crate::scene::Id;
use crate::ui::Ui;

const BOX: f32 = 16.;

/// Width of a checkbox with this label.
pub fn width(ui: &mut Ui, label: &str) -> f32 {
    let style = ui.style();
    BOX + 7. + ui.text(label, style).width + 2.
}

/// Paints at the left of `rect`. Toggles `checked` on click; returns true then.
/// `None` for an unknown state (shown as a dash, not clickable).
pub fn checkbox(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    checked: Option<&mut bool>,
    label: &str,
    enabled: bool,
) -> bool {
    let t = ui.theme;
    let enabled = enabled && checked.is_some();
    let on = checked.as_deref().copied();
    let b = Rect::new(rect.x, rect.y + (rect.h - BOX) / 2., BOX, BOX);
    let alpha = if enabled { 1. } else { 0.45 };
    match on {
        Some(true) => {
            ui.p.rounded(b, 4., t.accent.with_alpha(alpha));
            ui.p.icon(
                crate::icons::CHECK,
                b.center(12., 12.),
                t.accent_ink.with_alpha(alpha),
            );
        }
        Some(false) => {
            ui.p.bordered(b, 4., t.surface, 1., t.muted.with_alpha(alpha))
        }
        None => {
            ui.p.bordered(b, 4., t.surface, 1., t.muted.with_alpha(0.45));
            ui.p.fill(b.center(8., 2.), t.muted.with_alpha(0.45));
        }
    }
    let style = ui.style();
    let text = Rect::new(
        b.right() + 7.,
        rect.y,
        rect.right() - b.right() - 7.,
        rect.h,
    );
    ui.p.label(text, label, style, t.ink.with_alpha(alpha));
    if !enabled {
        return false;
    }
    let hit = Rect::new(
        rect.x,
        rect.y,
        (BOX + 7. + ui.text(label, style).width).min(rect.w),
        rect.h,
    );
    ui.p.hitbox(hit, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, b, 4.);
    if ui.clicked(id)
        && let Some(c) = checked
    {
        *c = !*c;
        return true;
    }
    false
}
