//! Keyboard focus order. Widgets register as focusable while they paint, so the
//! order is the paint order (like document order in a browser). Tab moves through
//! the list of the previous frame. While a modal layer has focusables, Tab stays in
//! the modal layers (a focus trap for dialogs).

use crate::geom::Rect;
use crate::scene::{Id, layer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusKind {
    /// Enter and Space click it: buttons, checkboxes, selects.
    Activate,
    /// Takes the keys itself: text fields.
    Input,
    /// One Tab stop for a group; arrow keys move inside it: tabs, radio groups.
    Group,
    /// Focused by a click, skipped by Tab: selectable text.
    Passive,
}

#[derive(Clone, Copy, Debug)]
pub struct Focusable {
    pub id: Id,
    pub kind: FocusKind,
    /// Where the focus ring goes.
    pub rect: Rect,
    pub radius: f32,
    pub clip: Rect,
    pub layer: usize,
}

#[derive(Default, Debug)]
pub struct FocusOrder {
    list: Vec<Focusable>,
}

impl FocusOrder {
    pub fn push(&mut self, f: Focusable) {
        self.list.push(f);
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn get(&self, id: Id) -> Option<&Focusable> {
        self.list.iter().find(|f| f.id == id)
    }

    /// The entries from `from` on (those registered since the list had that length).
    pub fn since(&self, from: usize) -> &[Focusable] {
        self.list.get(from..).unwrap_or(&[])
    }

    /// The lowest layer Tab reaches: the modal layers while they have focusables.
    pub fn scope(&self) -> usize {
        if self.list.iter().any(|f| f.layer >= layer::MODAL) {
            layer::MODAL
        } else {
            layer::BASE
        }
    }

    pub fn in_scope(&self, id: Id) -> bool {
        let scope = self.scope();
        self.get(id).is_some_and(|f| f.layer >= scope)
    }

    /// Where Tab (or Shift+Tab with `back`) goes from `current`. From a widget that is
    /// not a Tab stop (or out of scope, or gone), it starts at the first (last) stop,
    /// or right after the widget if it is still in the list.
    pub fn next(&self, current: Option<Id>, back: bool) -> Option<Id> {
        let scope = self.scope();
        let stop = |f: &Focusable| f.layer >= scope && f.kind != FocusKind::Passive;
        let n = self.list.len();
        if n == 0 {
            return None;
        }
        let at = current.and_then(|id| {
            self.list
                .iter()
                .position(|f| f.id == id && f.layer >= scope)
        });
        let order: Box<dyn Iterator<Item = usize>> = match (at, back) {
            (Some(i), false) => Box::new((1..=n).map(move |k| (i + k) % n)),
            (Some(i), true) => Box::new((1..=n).map(move |k| (i + n - k) % n)),
            (None, false) => Box::new(0..n),
            (None, true) => Box::new((0..n).rev()),
        };
        order.map(|i| &self.list[i]).find(|f| stop(f)).map(|f| f.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(items: &[(u64, FocusKind, usize)]) -> FocusOrder {
        let mut o = FocusOrder::default();
        for &(id, kind, layer) in items {
            o.push(Focusable {
                id: Id(id),
                kind,
                rect: Rect::default(),
                radius: 0.,
                clip: Rect::default(),
                layer,
            });
        }
        o
    }

    use FocusKind::*;

    #[test]
    fn tab_cycles_in_paint_order() {
        let o = order(&[(1, Activate, 0), (2, Input, 0), (3, Group, 0)]);
        assert_eq!(o.next(None, false), Some(Id(1)));
        assert_eq!(o.next(Some(Id(1)), false), Some(Id(2)));
        assert_eq!(o.next(Some(Id(3)), false), Some(Id(1)));
        assert_eq!(o.next(None, true), Some(Id(3)));
        assert_eq!(o.next(Some(Id(1)), true), Some(Id(3)));
        assert_eq!(o.next(Some(Id(2)), true), Some(Id(1)));
    }

    #[test]
    fn passive_widgets_are_skipped_but_keep_their_place() {
        let o = order(&[(1, Activate, 0), (2, Passive, 0), (3, Activate, 0)]);
        assert_eq!(o.next(Some(Id(1)), false), Some(Id(3)));
        // Focus is on the selectable text: Tab goes on from there.
        assert_eq!(o.next(Some(Id(2)), false), Some(Id(3)));
        assert_eq!(o.next(Some(Id(2)), true), Some(Id(1)));
    }

    #[test]
    fn modal_layers_trap_the_focus() {
        let o = order(&[
            (1, Activate, layer::BASE),
            (2, Input, layer::MODAL),
            (3, Activate, layer::MODAL),
            (4, Activate, layer::BASE),
        ]);
        assert_eq!(o.scope(), layer::MODAL);
        assert!(!o.in_scope(Id(1)));
        assert!(o.in_scope(Id(3)));
        // From a widget under the dialog: into the dialog.
        assert_eq!(o.next(Some(Id(1)), false), Some(Id(2)));
        assert_eq!(o.next(Some(Id(3)), false), Some(Id(2)));
        assert_eq!(o.next(Some(Id(2)), true), Some(Id(3)));
    }

    #[test]
    fn unknown_or_empty() {
        let o = order(&[(1, Activate, 0)]);
        assert_eq!(o.next(Some(Id(99)), false), Some(Id(1)));
        assert_eq!(o.next(Some(Id(1)), false), Some(Id(1)));
        assert_eq!(FocusOrder::default().next(None, false), None);
        assert_eq!(order(&[(1, Passive, 0)]).next(None, false), None);
    }
}
