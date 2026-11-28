//! Single choice: a segmented control (theme picker), a radio group, and a lone radio
//! button. A group is one Tab stop; the arrow keys pick within it.

use winit::keyboard::NamedKey;
use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::Rect;
use crate::scene::Id;
use crate::ui::Ui;

/// The choice the arrow keys make in a group of `n` while `id` has the focus.
/// `prev` and `next` are the keys for the two directions.
pub fn arrow_choice(
    ui: &mut Ui,
    id: Id,
    n: usize,
    selected: usize,
    prev: NamedKey,
    next: NamedKey,
) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let to = if ui.key_for(id, prev) {
        (selected + n - 1) % n
    } else if ui.key_for(id, next) {
        (selected + 1) % n
    } else if ui.key_for(id, NamedKey::Home) {
        0
    } else if ui.key_for(id, NamedKey::End) {
        n - 1
    } else {
        return None;
    };
    (to != selected).then_some(to)
}

/// Segments with an optional icon each, filling `rect`. Returns the chosen index.
pub fn segmented(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    options: &[(Option<u16>, &str)],
    selected: usize,
) -> Option<usize> {
    let t = ui.theme;
    ui.p.bordered(rect, 8., t.sunken, 1., t.line);
    let n = options.len().max(1) as f32;
    let w = (rect.w - 4.) / n;
    let mut out = None;
    for (i, (icon, label)) in options.iter().enumerate() {
        let r = Rect::new(rect.x + 2. + i as f32 * w, rect.y + 2., w, rect.h - 4.);
        let sid = id.with(i);
        let on = i == selected;
        if on {
            ui.p.shadow(r, 6., 2., t.shadow);
            ui.p.rounded(r, 6., t.surface);
            ui.focusable(id, FocusKind::Group, r, 6.);
        } else if ui.hovered(sid) {
            ui.p.rounded(r, 6., t.hover);
        }
        let style = ui.style().with_size(13.).weight(if on { 600 } else { 500 });
        let text = ui.text(label, style);
        let icon_w = if icon.is_some() { 21. } else { 0. };
        let x = r.x + ((r.w - icon_w - text.width) / 2.).max(4.);
        let fg = if on { t.ink } else { t.ink2 };
        if let Some(icon) = icon {
            ui.p.icon(*icon, Rect::new(x, r.y + (r.h - 15.) / 2., 15., 15.), fg);
        }
        ui.label(
            Rect::new(x + icon_w, r.y, r.right() - x - icon_w, r.h),
            label,
            style,
            fg,
        );
        ui.p.hitbox(r, sid, CursorIcon::Pointer);
        ui.focus_alias(sid, id);
        if ui.clicked(sid) && !on {
            out = Some(i);
        }
    }
    out.or_else(|| {
        arrow_choice(
            ui,
            id,
            options.len(),
            selected,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
        )
    })
}

/// Radio buttons in a column from the top of `rect`, `row_h` apart. Returns the
/// chosen index.
pub fn radio_group(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    row_h: f32,
    options: &[&str],
    selected: usize,
) -> Option<usize> {
    let mut out = None;
    for (i, label) in options.iter().enumerate() {
        let r = Rect::new(rect.x, rect.y + i as f32 * row_h, rect.w, row_h);
        let rid = id.with(i);
        let hit = paint_radio(ui, rid, r, i == selected, label);
        ui.focus_alias(rid, id);
        if i == selected {
            ui.focusable(id, FocusKind::Group, hit, 6.);
        }
        if ui.clicked(rid) && i != selected {
            out = Some(i);
        }
    }
    out.or_else(|| {
        arrow_choice(
            ui,
            id,
            options.len(),
            selected,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
        )
    })
    .or_else(|| {
        arrow_choice(
            ui,
            id,
            options.len(),
            selected,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
        )
    })
}

/// A radio button with a label at the left of `rect`. Returns true when chosen.
pub fn radio(ui: &mut Ui, id: Id, rect: Rect, on: bool, label: &str) -> bool {
    let hit = paint_radio(ui, id, rect, on, label);
    ui.focusable(id, FocusKind::Activate, hit, 6.);
    ui.clicked(id) && !on
}

/// Paints one radio button; returns its hit rect.
fn paint_radio(ui: &mut Ui, id: Id, rect: Rect, on: bool, label: &str) -> Rect {
    let t = ui.theme;
    let b = Rect::new(rect.x, rect.y + (rect.h - 16.) / 2., 16., 16.);
    if on {
        ui.p.rounded(b, 8., t.accent);
        ui.p.rounded(b.center(6., 6.), 3., t.accent_ink);
    } else {
        ui.p.bordered(
            b,
            8.,
            t.surface,
            1.,
            if ui.hovered(id) { t.ink2 } else { t.muted },
        );
    }
    let style = ui.style();
    let w = ui.text(label, style).width;
    ui.label(
        Rect::new(b.right() + 7., rect.y, rect.w - 23., rect.h),
        label,
        style,
        t.ink,
    );
    let hit = Rect::new(rect.x, rect.y, (23. + w).min(rect.w), rect.h);
    ui.p.hitbox(hit, id, CursorIcon::Pointer);
    hit.inset(-3., 2., -3., 2.)
}
