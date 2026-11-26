//! A single-line text input: the editing model (`EditBuffer`) plus layout, pointer
//! selection, horizontal scrolling and painting. `widgets::text_field` puts it in a box.

use winit::event::Ime;
use winit::window::CursorIcon;

use crate::geom::{Color, Point, Rect, rgb, rgba};
use crate::input::{Clipboard, KeyPress};
use crate::paint::Paint;
use crate::scene::Id;
use crate::text::{EditBuffer, TextLayout, TextStyle};

pub struct TextInput {
    pub id: Id,
    pub edit: EditBuffer,
    placeholder: String,
    pub style: TextStyle,
    pub color: Color,
    pub placeholder_color: Color,
    pub caret_color: Color,
    pub selection_color: Color,
    selecting: bool,
    /// Pixels the text is shifted left to keep the caret in view.
    scroll_x: f32,
    last_layout: Option<TextLayout>,
    last_line: Rect,
}

impl TextInput {
    pub fn new(id: Id, placeholder: &str, style: TextStyle, color: Color) -> Self {
        Self {
            id,
            edit: EditBuffer::default(),
            placeholder: placeholder.into(),
            style,
            color,
            placeholder_color: rgba(0x000000, 0.2),
            caret_color: rgb(0x0066ff),
            selection_color: rgba(0x3311ff, 0.19),
            selecting: false,
            scroll_x: 0.,
            last_layout: None,
            last_line: Rect::default(),
        }
    }

    pub fn content(&self) -> &str {
        &self.edit.content
    }

    /// Replace the whole content, as if typed.
    pub fn set_content(&mut self, text: &str) {
        self.edit.set_text(text);
    }

    pub fn set_placeholder(&mut self, text: &str) {
        if self.placeholder != text {
            self.placeholder = text.into();
        }
    }

    pub fn select_all(&mut self) {
        self.edit.select_all();
    }

    pub fn is_selecting(&self) -> bool {
        self.selecting
    }

    /// Handle a key press. None: not for this field. Some(changed): handled, and
    /// whether the content changed.
    pub fn key(&mut self, key: &KeyPress, clip: &mut Clipboard) -> Option<bool> {
        self.edit.key(key, clip)
    }

    /// Handle IME composition. Returns true if something changed.
    pub fn ime(&mut self, ime: &Ime) -> bool {
        self.edit.ime(ime)
    }

    /// A press in the field: a caret, a word (double click) or everything (triple).
    pub fn pointer_down(&mut self, pos: Point, shift: bool, clicks: u8) {
        let ix = self.index_for_position(pos);
        match clicks {
            0 | 1 => {
                self.selecting = true;
                if shift {
                    self.edit.select_to(ix)
                } else {
                    self.edit.move_to(ix)
                }
            }
            2 => self.edit.select_word(ix),
            _ => self.edit.select_all(),
        }
    }

    /// Extend a drag selection. Returns true if the selection changed.
    pub fn pointer_move(&mut self, pos: Point) -> bool {
        if !self.selecting {
            return false;
        }
        let old = self.edit.selected.clone();
        self.edit.select_to(self.index_for_position(pos));
        old != self.edit.selected
    }

    pub fn pointer_up(&mut self) {
        self.selecting = false;
    }

    /// Paint the content. `line` is the text line rect; `hit` the clickable area
    /// (empty when disabled). Sets the IME area while focused.
    pub fn paint(&mut self, p: &mut Paint, line: Rect, hit: Rect) {
        if hit.w > 0. {
            p.hitbox(hit, self.id, CursorIcon::Text);
        }
        let e = &self.edit;
        let (text, color) = if e.content.is_empty() {
            (self.placeholder.as_str(), self.placeholder_color)
        } else {
            (e.content.as_str(), self.color)
        };
        let layout = p.layout(text, self.style, None);
        let focused = p.is_focused(self.id);
        // Keep the caret in view; no scrolling once the text fits.
        let caret = if e.content.is_empty() {
            0.
        } else {
            layout.x_for_index(e.cursor())
        };
        if layout.width <= line.w {
            self.scroll_x = 0.;
        } else if focused {
            if caret - self.scroll_x > line.w - 2. {
                self.scroll_x = caret - line.w + 2.;
            } else if caret < self.scroll_x {
                self.scroll_x = caret;
            }
            self.scroll_x = self
                .scroll_x
                .clamp(0., (layout.width - line.w + 2.).max(0.));
        }
        let x0 = line.x - self.scroll_x;
        let y = line.y + (line.h - layout.height) / 2.;
        let selection = self.selection_color;
        let clip = if hit.w > 0. { hit } else { line.expand(2.) };
        p.clip(clip, |p| {
            if !e.selected.is_empty() && !e.content.is_empty() {
                for r in layout.selection_rects(e.selected.clone()) {
                    p.fill(Rect::new(x0 + r.x, line.y, r.w, line.h), selection);
                }
            }
            p.draw_text(&layout, x0, y, color);
            if let Some(m) = &e.marked {
                let (a, b) = (layout.x_for_index(m.start), layout.x_for_index(m.end));
                p.fill(
                    Rect::new(x0 + a, y + layout.baseline + 2., b - a, 1.),
                    self.color,
                );
            }
            if focused {
                if e.selected.is_empty() {
                    p.overlay(Rect::new(x0 + caret, line.y, 1.5, line.h), self.caret_color);
                }
                let start = e
                    .marked
                    .as_ref()
                    .map_or(caret, |m| layout.x_for_index(m.start));
                p.set_ime_area(Rect::new(x0 + start, line.y, 2., line.h));
            }
        });
        if !e.content.is_empty() {
            self.last_layout = Some(layout);
        } else {
            self.last_layout = None;
        }
        self.last_line = line;
    }

    fn index_for_position(&self, pos: Point) -> usize {
        let (Some(layout), b) = (&self.last_layout, self.last_line) else {
            return 0;
        };
        layout
            .index_for_x(pos.x - b.x + self.scroll_x)
            .min(self.edit.content.len())
    }
}
