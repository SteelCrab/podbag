//! The immediate-mode layer over `View`. Input is queued between frames; each frame
//! the app's `ui()` paints every widget, and widgets report clicks, drags, keys and
//! scrolls as they are drawn. A frame that consumed input paints once more, so the
//! state changed by that input shows (the usual immediate-mode double frame).
//!
//! The framework handles some keys before the app sees them: Tab and Shift+Tab move
//! the focus, Escape closes the open popup, and Enter or Space click the focused
//! button-like widget. After the app, Cmd/Ctrl+C copies the text selection if no
//! widget used the key.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use winit::event::Ime;
use winit::keyboard::NamedKey;

use crate::focus::{FocusKind, FocusOrder, Focusable};
use crate::geom::{Color, Point, Rect, rgb, rgba};
use crate::host::{Cx, Event, NavButton, View};
use crate::input::{KeyPress, ScrollDelta, shortcut_letter, shortcut_modifier};
use crate::paint::Paint;
use crate::scene::{Id, layer};
use crate::selection::Selection;
use crate::text::{TextLayout, TextStyle};

/// How long the pointer rests on something before its tooltip shows.
pub const TOOLTIP_DELAY: Duration = Duration::from_millis(600);

/// The colors widgets use. The app picks the values.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub page: Color,
    pub surface: Color,
    pub sunken: Color,
    pub hover: Color,
    pub ink: Color,
    pub ink2: Color,
    pub muted: Color,
    pub line: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub accent_soft: Color,
    pub accent_ink: Color,
    pub green: Color,
    pub green_soft: Color,
    pub amber: Color,
    pub amber_soft: Color,
    pub red: Color,
    pub red_soft: Color,
    pub danger_bg: Color,
    pub gray_soft: Color,
    pub shadow: Color,
    pub backdrop: Color,
    /// Base text size.
    pub text_size: f32,
}

impl Theme {
    pub fn light() -> Self {
        Self {
            page: rgb(0xf9f9f7),
            surface: rgb(0xffffff),
            sunken: rgb(0xf2f1ed),
            hover: rgb(0xefeee9),
            ink: rgb(0x0b0b0b),
            ink2: rgb(0x52514e),
            muted: rgb(0x6f6e69),
            line: rgb(0xe1e0d9),
            accent: rgb(0x2a78d6),
            accent_text: rgb(0x1d5fae),
            accent_soft: rgb(0xe5eefa),
            accent_ink: rgb(0xffffff),
            green: rgb(0x1b7f45),
            green_soft: rgb(0xe3f3e9),
            amber: rgb(0x8f5a00),
            amber_soft: rgb(0xfbf0d9),
            red: rgb(0xb8322d),
            red_soft: rgb(0xfbe6e5),
            danger_bg: rgb(0xb8322d),
            gray_soft: rgb(0xeeede8),
            shadow: rgba(0x000000, 0.12),
            backdrop: rgba(0x000000, 0.35),
            text_size: 14.,
        }
    }

    pub fn dark() -> Self {
        Self {
            page: rgb(0x0d0d0d),
            surface: rgb(0x1a1a19),
            sunken: rgb(0x242422),
            hover: rgb(0x262624),
            ink: rgb(0xf0efec),
            ink2: rgb(0xc3c2b7),
            muted: rgb(0x9c9b94),
            line: rgb(0x2c2c2a),
            accent: rgb(0x3987e5),
            accent_text: rgb(0x8ab8f3),
            accent_soft: rgb(0x1b2a3f),
            accent_ink: rgb(0xffffff),
            green: rgb(0x5fd08f),
            green_soft: rgb(0x17301f),
            amber: rgb(0xf0b54d),
            amber_soft: rgb(0x352a14),
            red: rgb(0xf47c76),
            red_soft: rgb(0x3a1b1a),
            danger_bg: rgb(0xc4413b),
            gray_soft: rgb(0x2a2a28),
            shadow: rgba(0x000000, 0.5),
            backdrop: rgba(0x000000, 0.35),
            text_size: 14.,
        }
    }

    /// The highlight behind selected text.
    pub fn selection(&self) -> Color {
        self.accent.with_alpha(0.28)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}

/// A press of the primary button.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    pub pos: Point,
    pub shift: bool,
    /// 1 for a click, 2 for a double click, 3 for a triple click.
    pub clicks: u8,
}

/// Input queued since the last frame.
#[derive(Default)]
struct Input {
    pressed: Option<(Press, Option<Id>)>,
    released: Option<(Point, Option<Id>)>,
    scroll: Vec<(Option<Id>, ScrollDelta)>,
    /// Key presses, and whether a widget used them.
    keys: Vec<(KeyPress, bool)>,
    ime: Vec<Ime>,
    /// Mouse side buttons released.
    nav: Vec<NavButton>,
}

impl Input {
    fn is_empty(&self) -> bool {
        self.pressed.is_none()
            && self.released.is_none()
            && self.scroll.is_empty()
            && self.keys.is_empty()
            && self.ime.is_empty()
            && self.nav.is_empty()
    }
}

#[derive(Default)]
struct Tooltips {
    /// The target under the pointer (or focused), and since when.
    hot: Option<(Id, Instant)>,
    /// A widget claimed `hot` this frame.
    seen: bool,
    /// Not shown again until the pointer leaves it (it was clicked).
    suppressed: Option<Id>,
    suppressed_seen: bool,
    /// Tooltip areas of the last frame and of this one (for repaints on pointer moves).
    areas: Vec<(Id, Rect, usize)>,
    areas_next: Vec<(Id, Rect, usize)>,
    /// The tooltip to paint at the end of this frame.
    show: Option<(Rect, String)>,
}

impl Tooltips {
    /// The tooltip area under a point, topmost first.
    fn at(&self, p: Point) -> Option<Id> {
        let mut best: Option<(Id, usize)> = None;
        for (id, r, l) in &self.areas {
            if r.contains(p) && best.is_none_or(|b| *l >= b.1) {
                best = Some((*id, *l));
            }
        }
        best.map(|b| b.0)
    }
}

/// Widget state kept across frames.
#[derive(Default)]
pub struct Memory {
    /// The widget the pointer went down on, while the button is held.
    active: Option<Id>,
    pointer_down: bool,
    /// The open popup (select menu) and the hitboxes inside it last frame.
    popup: Option<Id>,
    popup_ids: Vec<Id>,
    popup_ids_next: Vec<Id>,
    /// The keyboard-highlighted item of the open menu.
    pub(crate) popup_cursor: Option<usize>,
    /// The menu item the pointer was last on.
    pub(crate) popup_hover: Option<Id>,
    /// Measured content heights (dialogs, scroll views), from the last frame.
    pub heights: HashMap<Id, f32>,
    focus_prev: FocusOrder,
    focus_next: FocusOrder,
    /// Hit ids that focus another widget when pressed (tabs focus their group).
    alias_prev: HashMap<Id, Id>,
    alias_next: HashMap<Id, Id>,
    /// The focus came from the keyboard: show the focus ring.
    focus_visible: bool,
    /// Tab moved the focus this frame (scroll views bring it into view).
    focus_moved: bool,
    /// The one text selection of the window: its widget, the range and the text.
    selection: Option<(Id, Selection)>,
    selection_text: Option<String>,
    tips: Tooltips,
    /// Frames the framework asked for (tooltips, drags, toasts).
    repaint_at: Option<Instant>,
}

pub struct Ui<'a, 'b> {
    pub p: &'a mut Paint<'b>,
    pub theme: Theme,
    input: &'a mut Input,
    pub mem: &'a mut Memory,
    clicked: Option<Id>,
    pressed: Option<Id>,
    consumed: bool,
}

impl Ui<'_, '_> {
    pub fn bounds(&self) -> Rect {
        self.p.bounds()
    }

    pub fn pointer(&self) -> Option<Point> {
        self.p.pointer()
    }

    /// When this frame started.
    pub fn now(&self) -> Instant {
        self.p.now()
    }

    /// Pressed and released on `id` (a click), or activated with the keyboard, this frame.
    pub fn clicked(&mut self, id: Id) -> bool {
        if self.clicked == Some(id) {
            self.clicked = None;
            self.consumed = true;
            return true;
        }
        false
    }

    /// The pointer went down on `id` this frame.
    pub fn pressed(&mut self, id: Id) -> Option<Press> {
        match self.input.pressed {
            Some((press, Some(hit))) if hit == id && self.pressed == Some(id) => {
                self.pressed = None;
                self.consumed = true;
                Some(press)
            }
            _ => None,
        }
    }

    /// The pointer is held down on `id`.
    pub fn is_active(&self, id: Id) -> bool {
        self.mem.active == Some(id) && self.mem.pointer_down
    }

    /// The pointer was released this frame after going down on `id`.
    pub fn released(&self, id: Id) -> bool {
        self.input.released.is_some() && self.mem.active == Some(id)
    }

    pub fn hovered(&self, id: Id) -> bool {
        self.p.is_hovered(id)
    }

    /// Vertical scrolling aimed at `id` this frame, in pixels (wheel lines are `line_px`).
    pub fn take_scroll(&mut self, id: Id, line_px: f32) -> f32 {
        self.take_scroll_xy(id, line_px).1
    }

    /// Scrolling aimed at `id` this frame, (x, y) in pixels.
    pub fn take_scroll_xy(&mut self, id: Id, line_px: f32) -> (f32, f32) {
        let (mut dx, mut dy) = (0., 0.);
        self.input.scroll.retain(|(target, d)| {
            if *target != Some(id) {
                return true;
            }
            let (x, y) = d.pixels(line_px);
            dx += x;
            dy += y;
            false
        });
        if dx != 0. || dy != 0. {
            self.consumed = true;
        }
        (dx, dy)
    }

    /// Key presses not yet used by a widget, with their index for `consume_key`.
    pub fn keys(&self) -> impl Iterator<Item = (usize, &KeyPress)> {
        self.input
            .keys
            .iter()
            .enumerate()
            .filter(|(_, k)| !k.1)
            .map(|(i, k)| (i, &k.0))
    }

    /// The mouse side buttons released since the last frame; uses them up.
    pub fn take_nav(&mut self) -> Vec<NavButton> {
        let nav = std::mem::take(&mut self.input.nav);
        if !nav.is_empty() {
            self.consumed = true;
        }
        nav
    }

    pub fn consume_key(&mut self, i: usize) {
        if let Some(k) = self.input.keys.get_mut(i) {
            k.1 = true;
            self.consumed = true;
        }
    }

    /// A named key pressed this frame (and not used yet); uses it up.
    pub fn key_pressed(&mut self, key: NamedKey) -> Option<winit::keyboard::ModifiersState> {
        let (i, mods) = self
            .keys()
            .find(|(_, k)| k.is(key))
            .map(|(i, k)| (i, k.mods))?;
        self.consume_key(i);
        Some(mods)
    }

    /// A named key without Ctrl, Alt or Cmd, while `id` has the focus; uses it up.
    pub fn key_for(&mut self, id: Id, key: NamedKey) -> bool {
        if !self.has_focus(id) {
            return false;
        }
        let Some(i) = self
            .keys()
            .find(|(_, k)| k.is(key) && k.plain())
            .map(|(i, _)| i)
        else {
            return false;
        };
        self.consume_key(i);
        true
    }

    /// A shortcut letter with the platform modifier (Cmd or Ctrl); uses it up.
    pub fn shortcut(&mut self, letter: char, shift: bool) -> bool {
        let found = self.keys().find(|(_, k)| {
            shortcut_modifier(k.mods)
                && k.mods.shift_key() == shift
                && shortcut_letter(k) == Some(letter)
        });
        if let Some((i, _)) = found {
            self.consume_key(i);
            return true;
        }
        false
    }

    pub fn take_ime(&mut self) -> Vec<Ime> {
        if !self.input.ime.is_empty() {
            self.consumed = true;
        }
        std::mem::take(&mut self.input.ime)
    }

    pub fn has_focus(&self, id: Id) -> bool {
        self.p.is_focused(id)
    }

    pub fn focus(&mut self, id: Option<Id>) {
        self.p.set_focus(id);
    }

    /// The focus came from the keyboard (focus rings show).
    pub fn focus_visible(&self) -> bool {
        self.mem.focus_visible
    }

    /// Register `id` in the focus order. `ring` is where the focus ring goes. Call it
    /// every frame the widget is shown and enabled.
    pub fn focusable(&mut self, id: Id, kind: FocusKind, ring: Rect, radius: f32) {
        let f = Focusable {
            id,
            kind,
            rect: ring,
            radius,
            clip: self.p.current_clip(),
            layer: self.p.current_layer(),
        };
        self.mem.focus_next.push(f);
    }

    /// A press on hitbox `hit` focuses `target` (the parts of a group focus the group).
    pub fn focus_alias(&mut self, hit: Id, target: Id) {
        self.mem.alias_next.insert(hit, target);
    }

    pub fn repaint(&mut self) {
        self.p.request_repaint();
    }

    /// Get an `Event::Timer` (and `UiApp::tick`) no later than `at`.
    pub fn wake_at(&mut self, at: Instant) {
        self.p.wake_at(at);
    }

    /// Paint again no later than `at`, whatever `UiApp::tick` says.
    pub fn repaint_at(&mut self, at: Instant) {
        self.p.wake_at(at);
        if self.mem.repaint_at.is_none_or(|w| at < w) {
            self.mem.repaint_at = Some(at);
        }
    }

    /// Run `f` with an extra clip rect.
    pub fn clip<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.p.clip;
        self.p.clip = old.intersect(&rect);
        let r = f(self);
        self.p.clip = old;
        r
    }

    /// Run `f` on another layer, with the clip reset to the window.
    pub fn on_layer<R>(&mut self, layer: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        let (old_layer, old_clip) = (self.p.layer, self.p.clip);
        self.p.layer = layer.min(crate::scene::layer::COUNT - 1);
        self.p.clip = self.p.bounds;
        let r = f(self);
        self.p.layer = old_layer;
        self.p.clip = old_clip;
        r
    }

    // ---- popups ----

    pub fn popup_open(&self, id: Id) -> bool {
        self.mem.popup == Some(id)
    }

    pub fn open_popup(&mut self, id: Option<Id>) {
        self.mem.popup = id;
        self.mem.popup_cursor = None;
        self.mem.popup_hover = None;
        self.repaint();
    }

    /// Mark a hitbox as part of the open popup (clicks on it keep it open).
    pub fn popup_hitbox(&mut self, id: Id) {
        self.mem.popup_ids_next.push(id);
    }

    // ---- text selection ----

    /// The selection, if it is in widget `id`.
    pub fn selection(&self, id: Id) -> Option<Selection> {
        self.mem
            .selection
            .filter(|(owner, _)| *owner == id)
            .map(|(_, s)| s)
    }

    /// Set the window's selection: it is now in `id`. `text` is the selected text, for
    /// copying; pass None while a drag is still going (and set it on release).
    pub fn set_selection(&mut self, id: Id, selection: Selection, text: Option<String>) {
        self.mem.selection = Some((id, selection));
        if text.is_some() || selection.is_empty() {
            self.mem.selection_text = text.filter(|t| !t.is_empty());
        }
    }

    pub fn clear_selection(&mut self) {
        self.mem.selection = None;
        self.mem.selection_text = None;
    }

    /// The selected text of the window, if any.
    pub fn selected_text(&self) -> Option<&str> {
        self.mem.selection_text.as_deref()
    }

    /// Put the selected text on the clipboard. False if nothing is selected.
    pub fn copy_selection(&mut self) -> bool {
        let Some(text) = self.mem.selection_text.clone() else {
            return false;
        };
        self.p.clipboard().write(text);
        true
    }

    // ---- tooltips ----

    /// Show `text` near `anchor` after the pointer rests on it (or the keyboard focus
    /// is on `id`) for `TOOLTIP_DELAY`. Call it every frame, after the widget.
    pub fn tooltip(&mut self, id: Id, anchor: Rect, text: &str) {
        if text.is_empty() {
            return;
        }
        let area = anchor.intersect(&self.p.current_clip());
        self.mem
            .tips
            .areas_next
            .push((id, area, self.p.current_layer()));
        let pointed = self.p.hovered_area(anchor) && !self.mem.pointer_down;
        let focused = self.mem.focus_visible && self.has_focus(id);
        let tips = &mut self.mem.tips;
        if !(pointed || focused) {
            return;
        }
        if tips.suppressed == Some(id) {
            tips.suppressed_seen = true;
            return;
        }
        if tips.seen && tips.hot.is_some_and(|(h, _)| h != id) {
            return; // another tooltip claimed this frame
        }
        let now = self.p.now();
        let since = match tips.hot {
            Some((h, since)) if h == id => since,
            _ => {
                tips.hot = Some((id, now));
                now
            }
        };
        tips.seen = true;
        if now >= since + TOOLTIP_DELAY {
            tips.show = Some((anchor, text.to_string()));
        } else {
            self.repaint_at(since + TOOLTIP_DELAY);
        }
    }

    // ---- text helpers ----

    pub fn style(&self) -> TextStyle {
        TextStyle::new(self.theme.text_size).line_height((self.theme.text_size * 1.45).round())
    }

    pub fn text(&mut self, text: &str, style: TextStyle) -> TextLayout {
        self.p.layout(text, style, None)
    }

    /// `Paint::label` that shows the whole text in a tooltip when it does not fit.
    pub fn label(&mut self, rect: Rect, text: &str, style: TextStyle, color: Color) -> TextLayout {
        let layout = self.p.label(rect, text, style, color);
        if self.p.layout(text, style, None).width > rect.w + 0.5 {
            let id = Id::of((
                "label-tip",
                text,
                rect.x.round() as i32,
                rect.y.round() as i32,
            ));
            self.tooltip(id, rect, text);
        }
        layout
    }

    /// Wrapped text at (x, y); returns its height.
    pub fn paragraph(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        text: &str,
        style: TextStyle,
        color: Color,
    ) -> f32 {
        let layout = self.p.layout_wrapped(text, style, width);
        self.p.draw_text(&layout, x, y, color);
        layout.height
    }

    /// The focusables registered since the list had `from` entries (see `focus_mark`).
    pub(crate) fn focusables_since(&self, from: usize) -> &[Focusable] {
        self.mem.focus_next.since(from)
    }

    /// The current length of this frame's focus list.
    pub(crate) fn focus_mark(&self) -> usize {
        self.mem.focus_next.len()
    }

    pub(crate) fn focus_moved(&self) -> bool {
        self.mem.focus_moved
    }
}

/// An app written against `Ui`.
pub trait UiApp: 'static {
    type Message: Send + 'static;
    fn ui(&mut self, ui: &mut Ui);
    /// A message from another thread (backend events).
    fn message(&mut self, msg: Self::Message, cx: &mut Cx);
    /// A timer asked for with `wake_at` fired. Return true to paint.
    fn tick(&mut self, _cx: &mut Cx) -> bool {
        true
    }
    fn theme(&self, system_dark: bool) -> Theme;
    /// See `View::close_requested`. The default quits.
    fn close_requested(&mut self, cx: &mut Cx) {
        cx.quit();
    }
    /// See `View::reopen`. The default shows the window.
    fn reopen(&mut self, cx: &mut Cx) {
        cx.show_window();
    }
    /// See `View::exiting`.
    fn exiting(&mut self) {}
}

/// Runs a `UiApp` as a `View`.
pub struct UiView<A> {
    pub app: A,
    input: Input,
    mem: Memory,
    background: Color,
}

impl<A: UiApp> UiView<A> {
    pub fn new(app: A) -> Self {
        Self {
            app,
            input: Input::default(),
            mem: Memory::default(),
            background: rgb(0xffffff),
        }
    }

    /// Focus, selection and popups follow a press.
    fn on_press(&mut self, p: &mut Paint, hit: Option<Id>) {
        let mem = &mut self.mem;
        if mem.popup.is_some()
            && !hit.is_some_and(|h| mem.popup_ids.contains(&h) || Some(h) == mem.popup)
        {
            mem.popup = None;
        }
        if hit.is_some_and(|h| mem.popup_ids.contains(&h)) {
            return; // a menu item: the focus stays on the menu's button
        }
        let target = hit.and_then(|h| {
            mem.focus_prev
                .get(h)
                .or_else(|| mem.alias_prev.get(&h).and_then(|a| mem.focus_prev.get(*a)))
        });
        p.set_focus(target.map(|f| f.id));
        mem.focus_visible = false;
        // Clicking a button keeps the selection (it may be a Copy button); clicking
        // anything else clears it, and a selectable widget starts a new one.
        let keep = target.is_some_and(|f| f.kind == FocusKind::Activate);
        if !keep && mem.selection.is_some_and(|(owner, _)| Some(owner) != hit) {
            mem.selection = None;
            mem.selection_text = None;
        }
        if let Some(id) = mem.tips.hot.map(|h| h.0) {
            mem.tips.suppressed = Some(id);
            mem.tips.hot = None;
        }
    }

    /// Tab, Escape for popups, Enter and Space on the focused widget. Returns the
    /// widget a key clicked.
    fn framework_keys(&mut self, p: &mut Paint) -> Option<Id> {
        let mem = &mut self.mem;
        let mut clicked = None;
        for (key, used) in self.input.keys.iter_mut().filter(|(_, used)| !*used) {
            let plain = key.plain();
            if key.is(NamedKey::Tab) && plain {
                if let Some(next) = mem.focus_prev.next(p.focused(), key.mods.shift_key()) {
                    p.set_focus(Some(next));
                    mem.focus_moved = true;
                }
                mem.focus_visible = true;
                mem.popup = None;
                *used = true;
            } else if key.is(NamedKey::Escape) && mem.popup.is_some() {
                mem.popup = None;
                *used = true;
            } else if (key.is(NamedKey::Enter) || key.is(NamedKey::Space)) && plain && !key.repeat {
                let Some(f) = p.focused() else { continue };
                let activates = mem
                    .focus_prev
                    .get(f)
                    .is_some_and(|x| x.kind == FocusKind::Activate);
                if activates && mem.focus_prev.in_scope(f) && mem.popup != Some(f) {
                    clicked = Some(f);
                    *used = true;
                }
            }
        }
        clicked
    }

    /// After the app: copy, the focus ring, the tooltip.
    fn finish_frame(&mut self, p: &mut Paint, theme: &Theme) {
        let mem = &mut self.mem;
        let copy = self.input.keys.iter_mut().find(|(k, used)| {
            !*used
                && shortcut_modifier(k.mods)
                && !k.mods.shift_key()
                && shortcut_letter(k) == Some('c')
        });
        if let (Some((_, used)), Some(text)) = (copy, &mem.selection_text) {
            p.clipboard().write(text.clone());
            *used = true;
        }
        if mem.focus_visible
            && let Some(id) = p.focused()
            && mem.focus_next.in_scope(id)
            && let Some(f) = mem.focus_next.get(id).copied()
            && f.kind != FocusKind::Input
        {
            // A 2 px ring, 2 px away from the widget so it shows on accent fills too.
            p.ring(
                f.layer,
                f.rect.expand(4.),
                f.radius + 4.,
                2.,
                theme.accent,
                f.clip.expand(6.),
            );
        }
        let tips = &mut mem.tips;
        if !tips.seen {
            tips.hot = None;
        }
        if !tips.suppressed_seen {
            tips.suppressed = None;
        }
        if let Some((anchor, text)) = tips.show.take() {
            paint_tooltip(p, theme, anchor, &text);
        }
        std::mem::swap(&mut tips.areas, &mut tips.areas_next);
        tips.areas_next.clear();
        std::mem::swap(&mut mem.focus_prev, &mut mem.focus_next);
        std::mem::swap(&mut mem.alias_prev, &mut mem.alias_next);
        std::mem::swap(&mut mem.popup_ids, &mut mem.popup_ids_next);
    }
}

fn paint_tooltip(p: &mut Paint, t: &Theme, anchor: Rect, text: &str) {
    let style = TextStyle::new(12.5).line_height(17.);
    let b = p.bounds();
    let layout = p.layout_wrapped(text, style, 320f32.min(b.w - 32.));
    let (w, h) = (layout.width + 16., layout.height + 10.);
    let x = (anchor.x + (anchor.w - w) / 2.).clamp(b.x + 6., (b.right() - w - 6.).max(b.x + 6.));
    let below = anchor.bottom() + 6.;
    let y = if below + h <= b.bottom() - 6. {
        below
    } else {
        (anchor.y - 6. - h).max(b.y + 6.)
    };
    let r = Rect::new(x, y, w, h);
    p.on_layer(layer::TOP, |p| {
        p.shadow(r, 6., 8., t.shadow);
        p.rounded(r, 6., t.ink);
        p.draw_text(&layout, r.x + 8., r.y + 5., t.page);
    });
}

impl<A: UiApp> View for UiView<A> {
    type Message = A::Message;

    fn paint(&mut self, p: &mut Paint) {
        let theme = self.app.theme(p.system_dark());
        self.background = theme.page;
        let pressed = self.input.pressed.map(|(_, hit)| hit);
        if let Some(hit) = pressed {
            self.on_press(p, hit);
        }
        let mut clicked = match (self.input.pressed, self.input.released) {
            (Some((_, a)), Some((_, b))) if a == b => a,
            (None, Some((_, b))) if self.mem.active.is_some() && self.mem.active == b => b,
            _ => None,
        };
        self.mem.focus_moved = false;
        if let Some(id) = self.framework_keys(p) {
            clicked = Some(id);
        }
        let had_input = !self.input.is_empty();
        if self.mem.repaint_at.is_some_and(|at| at <= p.now()) {
            self.mem.repaint_at = None;
        }
        self.mem.popup_ids_next.clear();
        self.mem.focus_next.clear();
        self.mem.alias_next.clear();
        self.mem.tips.seen = false;
        self.mem.tips.suppressed_seen = false;
        let mut ui = Ui {
            p,
            theme,
            input: &mut self.input,
            mem: &mut self.mem,
            clicked,
            pressed: pressed.flatten(),
            consumed: false,
        };
        self.app.ui(&mut ui);
        let consumed = ui.consumed;
        self.finish_frame(p, &theme);
        if self.input.released.is_some() {
            self.mem.active = None;
        }
        self.input = Input::default();
        if had_input && consumed {
            p.request_repaint();
        }
    }

    fn event(&mut self, event: Event<A::Message>, cx: &mut Cx) {
        match event {
            Event::PointerDown {
                pos,
                hit,
                mods,
                clicks,
            } => {
                self.mem.active = hit;
                self.mem.pointer_down = true;
                self.input.pressed = Some((
                    Press {
                        pos,
                        shift: mods.shift_key(),
                        clicks,
                    },
                    hit,
                ));
                cx.notify();
            }
            Event::PointerUp { pos, hit } => {
                self.mem.pointer_down = false;
                self.input.released = Some((pos, hit));
                cx.notify();
            }
            Event::PointerMove { pos } => {
                let dragging = self.mem.pointer_down && self.mem.active.is_some();
                let tip = self.mem.tips.at(pos);
                if dragging || tip != self.mem.tips.hot.map(|h| h.0) {
                    cx.notify();
                }
            }
            Event::PointerLeft => {
                if self.mem.tips.hot.is_some() {
                    cx.notify();
                }
            }
            Event::Scroll { delta, target, .. } => {
                self.input.scroll.push((target, delta));
                cx.notify();
            }
            Event::Key(key) => {
                self.input.keys.push((key, false));
                cx.notify();
            }
            Event::Ime(ime) => {
                self.input.ime.push(ime);
                cx.notify();
            }
            Event::Nav(b) => {
                self.input.nav.push(b);
                cx.notify();
            }
            Event::Message(m) => self.app.message(m, cx),
            Event::Timer => {
                match self.mem.repaint_at {
                    Some(at) if at <= Instant::now() => cx.notify(),
                    Some(at) => cx.wake_at(at),
                    None => {}
                }
                if self.app.tick(cx) {
                    cx.notify();
                }
            }
        }
    }

    fn close_requested(&mut self, cx: &mut Cx) {
        self.app.close_requested(cx);
    }

    fn reopen(&mut self, cx: &mut Cx) {
        self.app.reopen(cx);
    }

    fn exiting(&mut self) {
        self.app.exiting();
    }

    fn repaint_on_hover_change(&self) -> bool {
        true
    }

    fn background(&self) -> Color {
        self.background
    }
}
