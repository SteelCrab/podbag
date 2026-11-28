//! A modal dialog: a backdrop over the window and a centered card on the modal layer.
//! While it shows, Tab moves only through its widgets.

use winit::keyboard::NamedKey;
use winit::window::CursorIcon;

use crate::geom::Rect;
use crate::scene::{Id, layer};
use crate::ui::Ui;

/// Paints a dialog `width` wide. `content(ui, inner)` paints from the top of `inner`
/// and returns the height it used; the card fits it from the next frame on.
/// Returns true when the user asks to close it (Escape that no widget inside used).
pub fn dialog(ui: &mut Ui, id: Id, width: f32, content: impl FnOnce(&mut Ui, Rect) -> f32) -> bool {
    let t = ui.theme;
    let bounds = ui.bounds();
    let w = width.min(bounds.w - 48.);
    let known = ui.mem.heights.get(&id).copied();
    let h = known.unwrap_or(160.) + 38.;
    let card = Rect::new(
        bounds.x + (bounds.w - w) / 2.,
        bounds.y + ((bounds.h - h) / 2.).max(24.),
        w,
        h.min(bounds.h - 48.),
    );
    let used = ui.on_layer(layer::MODAL, |ui| {
        ui.p.fill(bounds, t.backdrop);
        ui.p.hitbox(bounds, id.with("backdrop"), CursorIcon::Default);
        ui.p.shadow(card, 12., 20., t.shadow);
        ui.p.bordered(card, 12., t.surface, 1., t.line);
        ui.p.hitbox(card, id.with("card"), CursorIcon::Default);
        let inner = card.inset(22., 20., 22., 18.);
        ui.clip(card, |ui| {
            content(ui, Rect::new(inner.x, inner.y, inner.w, f32::MAX / 4.))
        })
    });
    if known.is_none_or(|k| (k - used).abs() > 0.5) {
        ui.mem.heights.insert(id, used);
        ui.repaint();
    }
    ui.key_pressed(NamedKey::Escape).is_some()
}

/// Right-aligned buttons along the bottom of a dialog: returns their rects, right to left.
pub fn buttons_row(x: f32, y: f32, w: f32, widths: &[f32]) -> Vec<Rect> {
    let mut right = x + w;
    widths
        .iter()
        .map(|bw| {
            right -= bw;
            let r = Rect::new(right, y, *bw, 32.);
            right -= 8.;
            r
        })
        .collect()
}
