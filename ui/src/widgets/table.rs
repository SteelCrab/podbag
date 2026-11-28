//! A table with a sticky header and a virtual list body (only visible rows are laid out).

use winit::window::CursorIcon;

use crate::geom::Rect;
use crate::scene::Id;
use crate::ui::Ui;
use crate::virtual_list::VirtualList;

#[derive(Clone, Copy)]
pub enum Width {
    Px(f32),
    /// A share of the width the fixed columns leave.
    Flex(f32),
}

pub struct Column<'a> {
    pub label: &'a str,
    pub width: Width,
}

pub const HEADER_H: f32 = 36.;
const PAD: f32 = 10.;

/// Column rects inside a row rect (each without its side padding).
pub fn cells(cols: &[Column], row: Rect) -> Vec<Rect> {
    let fixed: f32 = cols
        .iter()
        .map(|c| if let Width::Px(w) = c.width { w } else { 0. })
        .sum();
    let flex: f32 = cols
        .iter()
        .map(|c| if let Width::Flex(f) = c.width { f } else { 0. })
        .sum();
    let rest = (row.w - fixed).max(0.);
    let mut x = row.x;
    cols.iter()
        .map(|c| {
            let w = match c.width {
                Width::Px(w) => w,
                Width::Flex(f) => {
                    if flex > 0. {
                        rest * f / flex
                    } else {
                        0.
                    }
                }
            };
            let r = Rect::new(x + PAD, row.y, (w - 2. * PAD).max(0.), row.h);
            x += w;
            r
        })
        .collect()
}

/// Paints the table in `rect`. `cell(ui, row, col, rect)` paints one cell. Returns the
/// row clicked (not on a button inside it).
pub fn table(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    cols: &[Column],
    count: usize,
    list: &mut VirtualList,
    mut cell: impl FnMut(&mut Ui, usize, usize, Rect),
) -> Option<usize> {
    let t = ui.theme;
    let mut body = rect;
    let header = body.cut_top(HEADER_H);
    ui.p.border_bottom(header, t.line);
    let style = ui.style().with_size(12.).weight(600);
    for (c, r) in cols.iter().zip(cells(cols, header.inset(0., 2., 0., 0.))) {
        ui.label(r, c.label, style, t.muted);
    }
    let dy = ui.take_scroll(id, 40.);
    if dy != 0. {
        list.scroll_px(dy);
    }
    list.layout(body, count);
    ui.p.scroll_region(body, id);
    let mut clicked = None;
    ui.clip(body, |ui| {
        for i in list.visible() {
            let r = list.row_rect(i);
            let rid = id.with(("row", i));
            if ui.hovered(rid) || ui.p.hovered_area(r) {
                ui.p.rounded(r.inset(0., 0., 0., 1.), 7., t.hover);
            }
            ui.p.border_bottom(r, t.line);
            ui.p.hitbox(r, rid, CursorIcon::Pointer);
            for (c, cr) in cells(cols, r).into_iter().enumerate() {
                cell(ui, i, c, cr);
            }
            if ui.clicked(rid) {
                clicked = Some(i);
            }
        }
    });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flex_columns_share_what_fixed_ones_leave() {
        let cols = [
            Column {
                label: "a",
                width: Width::Px(100.),
            },
            Column {
                label: "b",
                width: Width::Flex(1.),
            },
            Column {
                label: "c",
                width: Width::Flex(3.),
            },
        ];
        let c = cells(&cols, Rect::new(0., 5., 500., 40.));
        // 400 px of flex: 100 and 300; each cell loses its side padding.
        assert_eq!(c[0], Rect::new(PAD, 5., 100. - 2. * PAD, 40.));
        assert_eq!(c[1], Rect::new(100. + PAD, 5., 100. - 2. * PAD, 40.));
        assert_eq!(c[2], Rect::new(200. + PAD, 5., 300. - 2. * PAD, 40.));
        // Too narrow: flex columns get nothing, never a negative width.
        let c = cells(&cols, Rect::new(0., 0., 50., 40.));
        assert_eq!(c[1].w, 0.);
    }
}
