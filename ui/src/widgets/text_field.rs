//! A bordered text field around `TextInput`.

use crate::focus::FocusKind;
use crate::geom::Rect;
use crate::text_input::TextInput;
use crate::ui::Ui;

/// Paints the field in `rect` with an optional leading icon. Handles clicks, drags,
/// keys and IME when focused. Returns true if the content changed.
pub fn text_field(
    ui: &mut Ui,
    input: &mut TextInput,
    rect: Rect,
    icon: Option<u16>,
    enabled: bool,
) -> bool {
    field(ui, input, rect, icon, enabled, true)
}

/// The same without a box (for inputs inside another frame).
pub fn plain_field(
    ui: &mut Ui,
    input: &mut TextInput,
    rect: Rect,
    icon: Option<u16>,
    enabled: bool,
) -> bool {
    field(ui, input, rect, icon, enabled, false)
}

fn field(
    ui: &mut Ui,
    input: &mut TextInput,
    rect: Rect,
    icon: Option<u16>,
    enabled: bool,
    frame: bool,
) -> bool {
    let t = ui.theme;
    let id = input.id;
    let focused = ui.has_focus(id);
    if frame {
        let border = if focused { t.accent } else { t.line };
        ui.p.bordered(rect, 7., t.surface, if focused { 1.5 } else { 1. }, border);
    }
    let pad = if frame { 10. } else { 0. };
    let mut inner = rect.inset(pad, 0., pad, 0.);
    if let Some(icon) = icon {
        ui.p.icon(
            icon,
            Rect::new(inner.x, rect.y + (rect.h - 16.) / 2., 16., 16.),
            t.muted,
        );
        inner = inner.inset(22., 0., 0., 0.);
    }
    input.color = if enabled {
        t.ink
    } else {
        t.ink.with_alpha(0.45)
    };
    input.placeholder_color = t.muted;
    input.caret_color = t.accent;
    input.selection_color = t.selection();
    let line = if input.edit.multiline {
        if enabled {
            ui.p.scroll_region(rect, id);
            let (dx, dy) = ui.take_scroll_xy(id, input.style.line_height);
            input.scroll(dx, dy);
        }
        inner.pad(0., 8.)
    } else {
        inner.center_v(input.style.line_height)
    };
    let mut changed = false;
    if enabled {
        ui.focusable(id, FocusKind::Input, rect, 7.);
        if let Some(press) = ui.pressed(id) {
            ui.focus(Some(id));
            input.pointer_down(press.pos, press.shift, press.clicks);
        } else if ui.is_active(id) {
            if let Some(pos) = ui.pointer()
                && input.pointer_move(pos)
            {
                ui.repaint();
            }
        } else if input.is_selecting() {
            input.pointer_up();
        }
        if ui.has_focus(id) {
            let keys: Vec<usize> = ui.keys().map(|(i, _)| i).collect();
            for i in keys {
                let Some((_, key)) = ui.keys().find(|(j, _)| *j == i) else {
                    continue;
                };
                let key = key.clone();
                if let Some(c) = input.key(&key, ui.p.clipboard()) {
                    changed |= c;
                    ui.consume_key(i);
                }
            }
            for ime in ui.take_ime() {
                changed |= input.ime(&ime);
            }
        }
    }
    let hit = if enabled { rect } else { Rect::default() };
    input.paint(ui.p, line, hit);
    changed
}
