//! An on/off switch. Space or Enter toggles it when focused.

use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::{Rect, rgb};
use crate::scene::Id;
use crate::ui::Ui;

pub const WIDTH: f32 = 44.;
pub const HEIGHT: f32 = 26.;

/// Paints a 44 x 26 switch at the top left of `rect`. Toggles `on` on click and
/// returns true then.
pub fn switch(ui: &mut Ui, id: Id, rect: Rect, on: &mut bool, enabled: bool) -> bool {
    let t = ui.theme;
    let r = Rect::new(rect.x, rect.y, WIDTH, HEIGHT);
    let alpha = if enabled { 1. } else { 0.5 };
    let track = if *on {
        t.accent
    } else {
        t.muted.with_alpha(0.4)
    };
    ui.p.rounded(r, HEIGHT / 2., track.with_alpha(alpha));
    let knob_x = if *on { r.x + 21. } else { r.x + 3. };
    let knob = Rect::new(knob_x, r.y + 3., 20., 20.);
    ui.p.shadow(
        knob.inset(0., 1., 0., -1.),
        10.,
        3.,
        rgb(0).with_alpha(0.3 * alpha),
    );
    ui.p.rounded(
        knob,
        10.,
        rgb(0xf7f6f3).with_alpha(if enabled { 1. } else { 0.8 }),
    );
    if !enabled {
        return false;
    }
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r, HEIGHT / 2.);
    if ui.clicked(id) {
        *on = !*on;
        return true;
    }
    false
}
