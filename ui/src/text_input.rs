//! An editable text field: the editing model (`EditBuffer`) plus layout, pointer
//! selection, scrolling and painting. `widgets::text_field` puts it in a box.

use winit::event::Ime;
use winit::keyboard::NamedKey;
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
    scroll_y: f32,
    reveal_caret: bool,
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
            scroll_y: 0.,
            reveal_caret: true,
            last_layout: None,
            last_line: Rect::default(),
        }
    }

    pub fn multiline(mut self) -> Self {
        self.edit.multiline = true;
        self
    }

    pub fn scroll(&mut self, dx: f32, dy: f32) {
        self.scroll_x -= dx;
        self.scroll_y -= dy;
    }

    pub fn content(&self) -> &str {
        &self.edit.content
    }

    /// Replace the whole content, as if typed.
    pub fn set_content(&mut self, text: &str) {
        self.edit.set_text(text);
        self.reveal_caret = true;
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
        self.reveal_caret = true;
        if self.edit.multiline
            && self.edit.marked.is_none()
            && key.plain()
            && (key.is(NamedKey::ArrowUp) || key.is(NamedKey::ArrowDown))
            && let Some(layout) = &self.last_layout
            && layout.text() == self.content()
        {
            let height = self.style.line_height;
            let caret = layout.position(self.edit.cursor());
            let y = caret.y + if key.is(NamedKey::ArrowUp) { -0.5 } else { 1.5 } * height;
            let next = layout.index_at(Point::new(caret.x, y));
            if key.mods.shift_key() {
                self.edit.select_to(next)
            } else {
                self.edit.move_to(next)
            }
            return Some(false);
        }
        self.edit.key(key, clip)
    }

    /// Handle IME composition. Returns true if something changed.
    pub fn ime(&mut self, ime: &Ime) -> bool {
        self.reveal_caret = true;
        self.edit.ime(ime)
    }

    /// A press in the field: a caret, a word (double click) or everything (triple).
    pub fn pointer_down(&mut self, pos: Point, shift: bool, clicks: u8) {
        self.reveal_caret = true;
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
        self.reveal_caret = true;
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
        let caret = if e.content.is_empty() {
            Point::default()
        } else {
            layout.position(e.cursor())
        };
        let lh = self.style.line_height;
        let multiline = e.multiline;
        if focused && (self.reveal_caret || !multiline) {
            self.scroll_x = keep_visible(self.scroll_x, caret.x, 2., line.w);
            self.scroll_y = keep_visible(self.scroll_y, caret.y, lh, line.h);
        }
        self.reveal_caret = false;
        self.scroll_x = self
            .scroll_x
            .clamp(0., (layout.width + 2. - line.w).max(0.));
        self.scroll_y = if multiline {
            self.scroll_y.clamp(0., (layout.height - line.h).max(0.))
        } else {
            0.
        };
        let x = line.x - self.scroll_x;
        let y = if multiline {
            line.y - self.scroll_y
        } else {
            line.y + (line.h - layout.height) / 2.
        };
        let clip = if multiline {
            line
        } else if hit.w > 0. {
            hit
        } else {
            line.expand(2.)
        };
        p.clip(clip, |p| {
            for r in layout.selection_rects(e.selected.clone()) {
                p.fill(Rect::new(x + r.x, y + r.y, r.w, r.h), self.selection_color);
            }
            p.draw_text(&layout, x, y, color);
            if let Some(m) = &e.marked {
                for r in layout.selection_rects(m.clone()) {
                    p.fill(
                        Rect::new(x + r.x, y + r.y + layout.baseline + 2., r.w, 1.),
                        self.color,
                    );
                }
            }
            if focused {
                if e.selected.is_empty() {
                    p.overlay(
                        Rect::new(x + caret.x, y + caret.y, 1.5, lh),
                        self.caret_color,
                    );
                }
                let start = e
                    .marked
                    .as_ref()
                    .map_or(caret, |m| layout.position(m.start));
                p.set_ime_area(Rect::new(x + start.x, y + start.y, 2., lh));
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
        let x = pos.x - b.x + self.scroll_x;
        let index = if self.edit.multiline {
            layout.index_at(Point::new(x, pos.y - b.y + self.scroll_y))
        } else {
            layout.index_for_x(x)
        };
        index.min(self.edit.content.len())
    }
}

fn keep_visible(offset: f32, pos: f32, size: f32, viewport: f32) -> f32 {
    offset.clamp((pos + size - viewport).min(pos), pos)
}
