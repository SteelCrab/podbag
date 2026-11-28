//! A log view: many monospace lines in a virtual list that can follow the end.
//!
//! Lines are selectable across lines (drag, double and triple click, Shift+click,
//! Cmd/Ctrl+A once focused) and copied with Cmd/Ctrl+C. Dragging past the top or
//! bottom scrolls. With `wrap` on, long lines wrap and each row gets its real height;
//! with it off, they stay on one line and scroll sideways.

use std::time::Duration;

use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::{Point, Rect};
use crate::scene::Id;
use crate::selection::TextPos;
use crate::text::{TextLayout, TextStyle};
use crate::ui::Ui;
use crate::virtual_list::VirtualList;
use crate::widgets::selectable::interact;

const LINE_H: f32 = 19.;
const KEEP: usize = 5000;

pub struct LogView {
    pub lines: Vec<String>,
    pub follow: bool,
    /// Wrap long lines instead of scrolling sideways.
    pub wrap: bool,
    flat: VirtualList,
    wrapped: VirtualList,
    wrap_width: f32,
    scroll_x: f32,
    widest: f32,
    new_lines: bool,
    /// Lines dropped from the front since the last paint (the selection moves up).
    dropped: usize,
    cleared: bool,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            follow: true,
            wrap: true,
            flat: VirtualList::new(LINE_H, LINE_H),
            wrapped: VirtualList::variable(LINE_H, LINE_H),
            wrap_width: 0.,
            scroll_x: 0.,
            widest: 0.,
            new_lines: false,
            dropped: 0,
            cleared: false,
        }
    }
}

impl LogView {
    /// Append lines; keeps the last 5000 once over 6000.
    pub fn push(&mut self, lines: impl IntoIterator<Item = String>) {
        self.lines.extend(lines);
        if self.lines.len() > KEEP + 1000 {
            let cut = self.lines.len() - KEEP;
            self.lines.drain(..cut);
            self.flat.drop_front(cut);
            self.wrapped.drop_front(cut);
            self.dropped += cut;
        }
        self.new_lines = true;
    }

    /// Replace everything with the lines of `text` (e.g. a JSON document).
    pub fn set_text(&mut self, text: &str) {
        self.clear();
        self.push(crate::text::split_lines(text).map(str::to_string));
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.flat = VirtualList::new(LINE_H, LINE_H);
        self.wrapped = VirtualList::variable(LINE_H, LINE_H);
        self.scroll_x = 0.;
        self.widest = 0.;
        self.cleared = true;
    }

    fn list(&mut self) -> &mut VirtualList {
        if self.wrap {
            &mut self.wrapped
        } else {
            &mut self.flat
        }
    }
}

/// Paints the lines in a bordered box; `empty` shows while there are none.
pub fn log_view(ui: &mut Ui, id: Id, rect: Rect, view: &mut LogView, empty: &str) {
    let t = ui.theme;
    ui.p.bordered(rect, 8., t.surface, 1., t.line);
    let inner = rect.inset(14., 12., 14., 12.);
    let style = TextStyle::new(12.5).mono().line_height(LINE_H);
    // The data changed under the selection: move it or drop it.
    if let Some(mut s) = ui.selection(id) {
        if view.cleared {
            ui.clear_selection();
        } else if view.dropped > 0 {
            s.drop_lines(view.dropped);
            let text = s.text(&view.lines);
            ui.set_selection(id, s, Some(text));
        }
    }
    view.dropped = 0;
    view.cleared = false;
    if view.lines.is_empty() {
        ui.p.label(inner.with_h(LINE_H), empty, style, t.ink2);
        return;
    }
    let (dx, dy) = ui.take_scroll_xy(id, LINE_H * 3.);
    if dy != 0. {
        view.list().scroll_px(dy);
    }
    if !view.wrap && dx != 0. {
        view.scroll_x = (view.scroll_x - dx).clamp(0., (view.widest - inner.w).max(0.));
    }
    let stick = view.follow && view.new_lines;
    view.new_lines = false;
    if view.wrap {
        if (view.wrap_width - inner.w).abs() > 0.5 {
            view.wrap_width = inner.w;
            view.wrapped.invalidate();
        }
        let lines = &view.lines;
        let p = &mut *ui.p;
        view.wrapped
            .layout_measured(inner, lines.len(), stick, |i| {
                p.layout_wrapped(&lines[i], style, inner.w).height
            });
        view.scroll_x = 0.;
    } else {
        view.flat.layout(inner, view.lines.len());
        if stick {
            view.flat.scroll_to_end();
        }
    }
    ui.p.scroll_region(rect, id);
    ui.p.hitbox(inner, id, CursorIcon::Text);
    ui.focusable(id, FocusKind::Passive, rect, 8.);

    // Pointer → (line, byte), for any line, in view or not (drags past the edges).
    let (selection, _) = {
        let LogView {
            lines,
            flat,
            wrapped,
            wrap,
            scroll_x,
            ..
        } = &mut *view;
        let list = if *wrap { wrapped } else { flat };
        interact(ui, id, lines.as_slice(), |ui, p| {
            let Some(i) = list.row_near(p.y) else {
                return TextPos::default();
            };
            let r = list.row_rect(i);
            let l = line_layout(ui, &lines[i], *wrap, style, inner.w);
            TextPos::new(i, l.index_at(Point::new(p.x - r.x + *scroll_x, p.y - r.y)))
        })
    };
    // Dragging past the top or bottom scrolls.
    if ui.is_active(id)
        && let Some(p) = ui.pointer()
    {
        let over = if p.y < inner.y {
            p.y - inner.y
        } else if p.y > inner.bottom() {
            p.y - inner.bottom()
        } else {
            0.
        };
        if over != 0. {
            view.list().scroll_px(-over.clamp(-60., 60.) / 2.);
            let now = ui.now();
            ui.repaint_at(now + Duration::from_millis(16));
        }
    }

    let range = view.list().visible();
    let sel_color = t.selection();
    let mut widest = view.widest;
    ui.clip(inner, |ui| {
        for i in range {
            let r = view.list().row_rect(i);
            let l = line_layout(ui, &view.lines[i], view.wrap, style, inner.w);
            widest = widest.max(l.width);
            let x = r.x - view.scroll_x;
            if let Some((sel, more)) = selection.and_then(|s| s.line_range(i, view.lines[i].len()))
            {
                let mut rects = l.selection_rects(sel.clone());
                if more {
                    // The line break is selected too: mark it after the text.
                    let end = l.position(view.lines[i].len());
                    rects.push(Rect::new(end.x, end.y, style.size * 0.35, LINE_H));
                }
                for s in rects {
                    ui.p.fill(s.translate(x, r.y), sel_color);
                }
            }
            ui.p.draw_text(&l, x, r.y, t.ink);
        }
    });
    view.widest = widest;
    // A thin thumb shows where the view is.
    let list = view.list();
    let (content, offset) = (list.content_height(), list.offset());
    if content > inner.h + 0.5 {
        let thumb_h = (inner.h * inner.h / content).max(20.);
        let y = inner.y + (inner.h - thumb_h) * (offset / (content - inner.h));
        ui.p.rounded(
            Rect::new(rect.right() - 7., y, 4., thumb_h),
            2.,
            t.muted.with_alpha(0.4),
        );
    }
}

fn line_layout(ui: &mut Ui, line: &str, wrap: bool, style: TextStyle, width: f32) -> TextLayout {
    if wrap {
        ui.p.layout_wrapped(line, style, width)
    } else {
        ui.p.layout(line, style, None)
    }
}

/// Scroll to the end now (e.g. when "follow" is turned on).
pub fn follow_now(view: &mut LogView) {
    view.list().scroll_to_end();
}
