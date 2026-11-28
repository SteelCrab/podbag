//! A horizontal slider. With the focus, the arrow keys move it by a step and Home
//! and End go to the ends.

use winit::keyboard::NamedKey;
use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::Rect;
use crate::scene::Id;
use crate::ui::Ui;

/// Drag or click to set `value` in [min, max], snapped to `step`. Returns true on change.
pub fn slider(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    value: &mut f32,
    min: f32,
    max: f32,
    step: f32,
) -> bool {
    let t = ui.theme;
    let track = Rect::new(rect.x + 8., rect.y + rect.h / 2. - 2., rect.w - 16., 4.);
    let frac = if max > min {
        ((*value - min) / (max - min)).clamp(0., 1.)
    } else {
        0.
    };
    ui.p.rounded(track, 2., t.sunken);
    ui.p.rounded(track.with_w(track.w * frac), 2., t.ink2);
    let knob = Rect::new(
        track.x + track.w * frac - 8.,
        rect.y + rect.h / 2. - 8.,
        16.,
        16.,
    );
    ui.p.shadow(knob, 8., 2., t.shadow);
    ui.p.bordered(knob, 8., t.surface, 1., t.line);
    ui.p.hitbox(rect, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Group, knob, 8.);
    let key_step = if step > 0. { step } else { (max - min) / 100. };
    let v = if let Some(press) = ui.pressed(id) {
        Some(from_x(press.pos.x, track, min, max))
    } else if ui.is_active(id) {
        ui.pointer().map(|p| from_x(p.x, track, min, max))
    } else if ui.key_for(id, NamedKey::ArrowLeft) || ui.key_for(id, NamedKey::ArrowDown) {
        Some(*value - key_step)
    } else if ui.key_for(id, NamedKey::ArrowRight) || ui.key_for(id, NamedKey::ArrowUp) {
        Some(*value + key_step)
    } else if ui.key_for(id, NamedKey::Home) {
        Some(min)
    } else if ui.key_for(id, NamedKey::End) {
        Some(max)
    } else {
        None
    };
    let Some(mut v) = v else { return false };
    if step > 0. {
        v = min + ((v - min) / step).round() * step;
    }
    let v = v.clamp(min, max);
    if (v - *value).abs() > f32::EPSILON {
        *value = v;
        ui.repaint();
        return true;
    }
    false
}

fn from_x(x: f32, track: Rect, min: f32, max: f32) -> f32 {
    let f = ((x - track.x) / track.w).clamp(0., 1.);
    min + f * (max - min)
}
