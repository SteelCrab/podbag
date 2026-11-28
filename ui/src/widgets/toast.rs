//! Toasts: short messages in the bottom right corner that go away by themselves.

use std::time::{Duration, Instant};

use crate::geom::{Color, Rect};
use crate::scene::layer;
use crate::ui::Ui;

struct Toast {
    text: String,
    error: bool,
    until: Instant,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    /// Show a message: 4 s, or 8 s for an error.
    pub fn push(&mut self, text: impl Into<String>, error: bool) {
        let secs = if error { 8 } else { 4 };
        self.items.push(Toast {
            text: text.into(),
            error,
            until: Instant::now() + Duration::from_secs(secs),
        });
    }

    /// Dismiss routine messages while keeping errors on screen.
    pub fn dismiss_routine(&mut self) {
        self.items.retain(|item| item.error);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The messages shown, oldest first, with whether each is an error.
    pub fn texts(&self) -> impl Iterator<Item = (&str, bool)> {
        self.items.iter().map(|t| (t.text.as_str(), t.error))
    }

    /// Drop the toasts whose time is up; true if any went.
    pub fn expire(&mut self) -> bool {
        let now = Instant::now();
        let before = self.items.len();
        self.items.retain(|t| t.until > now);
        self.items.len() != before
    }

    pub fn paint(&mut self, ui: &mut Ui) {
        let now = Instant::now();
        self.items.retain(|t| t.until > now);
        if let Some(next) = self.items.iter().map(|t| t.until).min() {
            ui.repaint_at(next);
        }
        let t = ui.theme;
        let bounds = ui.bounds();
        let max_w = 420f32.min(bounds.w - 36.);
        let style = ui.style().with_size(13.5);
        let mut bottom = bounds.bottom() - 18.;
        ui.on_layer(layer::TOP, |ui| {
            for toast in self.items.iter().rev() {
                let layout = ui.p.layout_wrapped(&toast.text, style, max_w - 28.);
                let (w, h) = (layout.width + 28., layout.height + 20.);
                let r = Rect::new(bounds.right() - 18. - w, bottom - h, w, h);
                let (bg, fg) = if toast.error {
                    (
                        t.danger_bg,
                        Color {
                            r: 1.,
                            g: 1.,
                            b: 1.,
                            a: 1.,
                        },
                    )
                } else {
                    (t.ink, t.page)
                };
                ui.p.shadow(r, 9., 14., t.shadow);
                ui.p.rounded(r, 9., bg);
                ui.p.draw_text(&layout, r.x + 14., r.y + 10., fg);
                bottom = r.y - 8.;
            }
        });
    }
}
