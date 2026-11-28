//! Read-only text that can be selected and copied (detail values, JSON, messages).
//!
//! Drag to select, double-click for a word, triple-click for a line, Shift+click
//! to extend, Cmd/Ctrl+A to select the whole block once it has the focus (a click
//! gives it), Cmd/Ctrl+C to copy. `Ui::selected_text` and `Ui::copy_selection`
//! give the selection to the app, e.g. for a Copy button.

use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::{Color, Point, Rect};
use crate::scene::Id;
use crate::selection::{Lines, Selection, TextPos};
use crate::text::{TextLayout, TextStyle};
use crate::ui::Ui;

/// Wrapped, selectable text with its top-left at `at`, `width` wide. Returns its height.
pub fn selectable_text(
    ui: &mut Ui,
    id: Id,
    at: Point,
    width: f32,
    text: &str,
    style: TextStyle,
    color: Color,
) -> f32 {
    let layout = ui.p.layout_wrapped(text, style, width);
    block(
        ui,
        id,
        Rect::new(at.x, at.y, width, layout.height),
        &layout,
        text,
        color,
    );
    layout.height
}

/// One line of selectable text, vertically centered in `rect` and clipped to it
/// (no ellipsis: what is cut off can still be selected and copied).
pub fn selectable_line(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    text: &str,
    style: TextStyle,
    color: Color,
) {
    let layout = ui.p.layout(text, style, None);
    let y = rect.y + (rect.h - layout.height) / 2.;
    let area = Rect::new(rect.x, y, rect.w, layout.height);
    ui.clip(rect, |ui| block(ui, id, area, &layout, text, color));
}

fn block(ui: &mut Ui, id: Id, area: Rect, layout: &TextLayout, text: &str, color: Color) {
    ui.p.hitbox(area, id, CursorIcon::Text);
    ui.focusable(id, FocusKind::Passive, area.expand(2.), 4.);
    let origin = Point::new(area.x, area.y);
    let (selection, _) = interact(ui, id, text, |_, p| {
        TextPos::new(
            0,
            layout.index_at(Point::new(p.x - origin.x, p.y - origin.y)),
        )
    });
    if let Some((range, _)) = selection.and_then(|s| s.line_range(0, text.len())) {
        let c = ui.theme.selection();
        for r in layout.selection_rects(range) {
            ui.p.fill(r.translate(origin.x, origin.y), c);
        }
    }
    ui.p.draw_text(layout, origin.x, origin.y, color);
}

/// Pointer and keyboard handling shared by the selectable widgets. `hit` maps a
/// window point to a position in `text`. Returns the selection in `id`, if any,
/// and whether it changed this frame.
pub(crate) fn interact<T: Lines + ?Sized>(
    ui: &mut Ui,
    id: Id,
    text: &T,
    mut hit: impl FnMut(&mut Ui, Point) -> TextPos,
) -> (Option<Selection>, bool) {
    let before = ui.selection(id);
    let mut sel = before;
    if let Some(press) = ui.pressed(id) {
        ui.focus(Some(id));
        let at = hit(ui, press.pos);
        sel = Some(match sel {
            Some(mut s) if press.shift => {
                s.extend(text, at);
                s
            }
            _ => Selection::begin(text, at, press.clicks),
        });
    } else if ui.is_active(id)
        && let (Some(p), Some(mut s)) = (ui.pointer(), sel)
    {
        s.extend(text, hit(ui, p));
        sel = Some(s);
    }
    if ui.has_focus(id) && ui.shortcut('a', false) {
        sel = Some(Selection::all(text));
    }
    let changed = sel != before;
    let released = ui.released(id);
    if let Some(s) = sel
        && (changed || released)
    {
        // The text is built when the drag ends, not on every move.
        let done = released || !ui.is_active(id);
        ui.set_selection(id, s, done.then(|| s.text(text)));
    }
    (sel, changed)
}
