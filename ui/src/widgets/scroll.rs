//! A vertical scroll view for content of any height. When Tab moves the focus to a
//! widget inside that is out of view, the view scrolls to it.

use crate::geom::Rect;
use crate::scene::Id;
use crate::ui::Ui;

#[derive(Default)]
pub struct ScrollState {
    pub offset: f32,
    content: f32,
    viewport: f32,
}

impl ScrollState {
    pub fn to_top(&mut self) {
        self.offset = 0.;
    }

    pub fn to_bottom(&mut self) {
        self.offset = (self.content - self.viewport).max(0.);
    }

    /// The offset that shows `top..bottom` (content coordinates), moving the least.
    fn reveal(&self, top: f32, bottom: f32) -> f32 {
        if top < self.offset {
            top
        } else if bottom > self.offset + self.viewport {
            (bottom - self.viewport).min(top)
        } else {
            self.offset
        }
    }
}

/// Paints `content(ui, origin_rect)` clipped to `rect`, shifted by the scroll offset.
/// `content` gets the full-width rect starting at the (scrolled) top and returns the
/// content height.
pub fn scroll_view(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    state: &mut ScrollState,
    content: impl FnOnce(&mut Ui, Rect) -> f32,
) {
    let dy = ui.take_scroll(id, 40.);
    state.offset -= dy;
    let max = (state.content - rect.h).max(0.);
    state.offset = state.offset.clamp(0., max);
    ui.p.scroll_region(rect, id);
    let origin = Rect::new(rect.x, rect.y - state.offset, rect.w, rect.h);
    let mark = ui.focus_mark();
    let h = ui.clip(rect, |ui| content(ui, origin));
    if (h - state.content).abs() > 0.5 || (rect.h - state.viewport).abs() > 0.5 {
        state.content = h;
        state.viewport = rect.h;
        let max = (h - rect.h).max(0.);
        if state.offset > max {
            state.offset = max;
            ui.repaint();
        }
    }
    if ui.focus_moved()
        && let Some(f) = ui
            .focusables_since(mark)
            .iter()
            .find(|f| ui.has_focus(f.id))
    {
        let top = f.rect.y - origin.y - 8.;
        let to = state
            .reveal(top, f.rect.bottom() - origin.y + 8.)
            .clamp(0., (h - rect.h).max(0.));
        if (to - state.offset).abs() > 0.5 {
            state.offset = to;
            ui.repaint();
        }
    }
    if h > rect.h + 0.5 {
        let t = ui.theme;
        let thumb_h = (rect.h * rect.h / h).max(24.);
        let y = rect.y + (rect.h - thumb_h) * (state.offset / (h - rect.h));
        ui.p.rounded(
            Rect::new(rect.right() - 7., y + 2., 5., thumb_h - 4.),
            2.5,
            t.muted.with_alpha(0.45),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_moves_the_least() {
        let s = ScrollState {
            offset: 100.,
            content: 1000.,
            viewport: 200.,
        };
        assert_eq!(s.reveal(150., 180.), 100.);
        assert_eq!(s.reveal(50., 80.), 50.);
        assert_eq!(s.reveal(320., 350.), 150.);
        // Taller than the view: its top shows.
        assert_eq!(s.reveal(400., 900.), 400.);
    }
}
