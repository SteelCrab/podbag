//! A drop-down select: a button showing the choice, and a menu on the popup layer.
//! Keyboard: Enter, Space or the arrow keys open it; in the menu the arrows, Home
//! and End move, Enter or Space choose, Escape and Tab close it.

use winit::keyboard::NamedKey;
use winit::window::CursorIcon;

use crate::focus::FocusKind;
use crate::geom::Rect;
use crate::icons;
use crate::scene::{Id, layer};
use crate::ui::Ui;

pub enum Item<'a> {
    Option(&'a str),
    /// A group heading (not choosable).
    Group(&'a str),
}

impl Item<'_> {
    fn choosable(&self) -> bool {
        matches!(self, Item::Option(_))
    }
}

const ITEM_H: f32 = 28.;

/// Returns the index of the chosen item (options and groups count alike).
pub fn select(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    items: &[Item],
    selected: usize,
    enabled: bool,
) -> Option<usize> {
    let t = ui.theme;
    let open = ui.popup_open(id);
    let hovered = enabled && ui.hovered(id);
    let border = if open { t.accent } else { t.line };
    ui.p.bordered(
        rect,
        7.,
        if hovered { t.hover } else { t.surface },
        1.,
        border,
    );
    let current = match items.get(selected) {
        Some(Item::Option(s)) => *s,
        _ => "",
    };
    let style = ui.style();
    let fg = if enabled {
        t.ink
    } else {
        t.ink.with_alpha(0.45)
    };
    ui.label(rect.inset(10., 0., 30., 0.), current, style, fg);
    ui.p.icon(
        icons::CHEVRON_DOWN,
        Rect::new(rect.right() - 24., rect.y + (rect.h - 14.) / 2., 14., 14.),
        t.muted,
    );
    if !enabled {
        return None;
    }
    ui.p.hitbox(rect, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, rect, 7.);
    if ui.clicked(id) {
        ui.open_popup(if open { None } else { Some(id) });
        return None;
    }
    if !open {
        if ui.key_for(id, NamedKey::ArrowDown) || ui.key_for(id, NamedKey::ArrowUp) {
            ui.open_popup(Some(id));
        }
        return None;
    }
    // Keyboard moves in the open menu.
    let mut cursor = ui
        .mem
        .popup_cursor
        .unwrap_or(selected)
        .min(items.len().saturating_sub(1));
    let step = |from: usize, down: bool| -> usize {
        let n = items.len();
        (1..=n)
            .map(|k| {
                if down {
                    (from + k) % n
                } else {
                    (from + n - k) % n
                }
            })
            .find(|i| items[*i].choosable())
            .unwrap_or(from)
    };
    if ui.key_for(id, NamedKey::ArrowDown) {
        cursor = step(cursor, true);
    } else if ui.key_for(id, NamedKey::ArrowUp) {
        cursor = step(cursor, false);
    } else if ui.key_for(id, NamedKey::Home) {
        cursor = step(items.len() - 1, true);
    } else if ui.key_for(id, NamedKey::End) {
        cursor = step(0, false);
    }
    let mut chosen = None;
    if (ui.key_for(id, NamedKey::Enter) || ui.key_for(id, NamedKey::Space))
        && items.get(cursor).is_some_and(Item::choosable)
    {
        chosen = Some(cursor);
    }
    // The menu, below the button (or above it when there is no room).
    let h = items.len() as f32 * ITEM_H + 8.;
    let bounds = ui.bounds();
    let w = rect.w.max(160.);
    let mut menu = Rect::new(
        rect.x.min(bounds.right() - w - 8.),
        rect.bottom() + 4.,
        w,
        h.min(bounds.h - 16.),
    );
    if menu.bottom() > bounds.bottom() - 8. {
        menu.y = (rect.y - 4. - menu.h).max(8.);
    }
    let popup_layer = if ui.p.current_layer() >= layer::MODAL {
        layer::MODAL_POPUP
    } else {
        layer::POPUP
    };
    ui.p.on_layer(popup_layer, |p| {
        p.shadow(menu, 9., 14., t.shadow);
        p.bordered(menu, 9., t.surface, 1., t.line);
        p.hitbox(menu, id.with("menu"), CursorIcon::Default);
    });
    ui.popup_hitbox(id.with("menu"));
    for (i, item) in items.iter().enumerate() {
        let r = Rect::new(
            menu.x + 4.,
            menu.y + 4. + i as f32 * ITEM_H,
            menu.w - 8.,
            ITEM_H,
        );
        let iid = id.with(("item", i));
        // The pointer moves the highlight when it moves onto an item, not while it
        // rests and the keys move it.
        if ui.hovered(iid) && item.choosable() && ui.mem.popup_hover != Some(iid) {
            ui.mem.popup_hover = Some(iid);
            cursor = i;
        }
        ui.popup_hitbox(iid);
        match item {
            Item::Group(label) => {
                let s = style.with_size(11.5).weight(600);
                ui.p.on_layer(popup_layer, |p| {
                    p.label(r.inset(8., 0., 8., 0.), label, s, t.muted);
                });
            }
            Item::Option(label) => {
                let lit = i == cursor;
                ui.p.on_layer(popup_layer, |p| {
                    if lit {
                        p.rounded(r, 6., t.accent_soft);
                    }
                    let fg = if lit { t.accent_text } else { t.ink };
                    p.label(r.inset(26., 0., 8., 0.), label, style, fg);
                    if i == selected {
                        p.icon(
                            icons::CHECK,
                            Rect::new(r.x + 7., r.y + (r.h - 13.) / 2., 13., 13.),
                            t.accent_text,
                        );
                    }
                    p.hitbox(r, iid, CursorIcon::Pointer);
                });
                if ui.clicked(iid) {
                    chosen = Some(i);
                }
            }
        }
    }
    ui.mem.popup_cursor = Some(cursor);
    if chosen.is_some() {
        ui.open_popup(None);
        ui.focus(Some(id));
    }
    chosen
}
