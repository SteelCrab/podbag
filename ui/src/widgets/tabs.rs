//! A row of tabs with an underline on the chosen one. The row is one Tab stop; the
//! left and right arrow keys (and Home, End) choose a tab.

use winit::keyboard::NamedKey;
use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::Rect;
use crate::scene::Id;
use crate::ui::Ui;
use crate::widgets::radio::arrow_choice;

/// Paints tabs from the left of `rect`; returns the chosen index.
pub fn tabs(ui: &mut Ui, id: Id, rect: Rect, labels: &[String], selected: usize) -> Option<usize> {
    let t = ui.theme;
    let style = ui.style().weight(500);
    let mut x = rect.x;
    let mut out = None;
    for (i, label) in labels.iter().enumerate() {
        let text = ui.text(label, style);
        let r = Rect::new(x, rect.y, text.width + 24., rect.h);
        let tid = id.with(i);
        let on = i == selected;
        let fg = if on {
            t.accent_text
        } else if ui.hovered(tid) {
            t.ink
        } else {
            t.ink2
        };
        ui.p.label(r.inset(12., 0., 0., 0.), label, style, fg);
        if on {
            ui.p.fill(Rect::new(r.x, r.bottom() - 2., r.w, 2.), t.accent);
            ui.focusable(id, FocusKind::Group, r.inset(2., 6., 2., 6.), 6.);
        }
        ui.p.hitbox(r, tid, CursorIcon::Pointer);
        ui.focus_alias(tid, id);
        if ui.clicked(tid) && !on {
            out = Some(i);
        }
        x += r.w + 4.;
    }
    out.or_else(|| {
        arrow_choice(
            ui,
            id,
            labels.len(),
            selected,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
        )
    })
}

/// Total width the tabs take.
pub fn width(ui: &mut Ui, labels: &[String]) -> f32 {
    let style = ui.style().weight(500);
    labels.iter().map(|l| ui.text(l, style).width + 28.).sum()
}
