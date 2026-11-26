//! The window-independent part of a running view: input dispatch, frames, hover and
//! focus. `app` drives it from winit and renders the scene with the GPU; `testing`
//! drives it directly, without a window.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use winit::event::Ime;
use winit::keyboard::ModifiersState;
use winit::window::CursorIcon;

use crate::geom::{Color, Point, Rect};
use crate::input::{Clipboard, KeyPress, ScrollDelta};
use crate::paint::{FrameCallback, Paint};
use crate::scene::{Id, Scene};
use crate::text::TextSystem;

/// Presses this close in time and space count as a double (triple) click.
const MULTI_CLICK: Duration = Duration::from_millis(500);
const MULTI_CLICK_SLOP: f32 = 4.;

/// The mouse's side buttons (4 and 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavButton {
    Back,
    Forward,
}

/// What the app asked of its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowRequest {
    /// Show it, unminimized and in front, with the keyboard.
    Show,
    Hide,
}

pub enum Event<M> {
    /// The primary button went down. `clicks`: 1, 2 for a double click, 3...
    PointerDown {
        pos: Point,
        hit: Option<Id>,
        mods: ModifiersState,
        clicks: u8,
    },
    PointerUp {
        pos: Point,
        hit: Option<Id>,
    },
    PointerMove {
        pos: Point,
    },
    /// The pointer left the window.
    PointerLeft,
    /// `target`: the topmost scroll region under the pointer.
    Scroll {
        pos: Point,
        delta: ScrollDelta,
        target: Option<Id>,
    },
    Key(KeyPress),
    Ime(Ime),
    /// A side button of the mouse was released.
    Nav(NavButton),
    Message(M),
    /// A time asked for with `wake_at` was reached.
    Timer,
}

/// A screen: paints itself and reacts to events.
pub trait View: 'static {
    type Message: Send + 'static;
    fn paint(&mut self, p: &mut Paint);
    fn event(&mut self, event: Event<Self::Message>, cx: &mut Cx);
    /// The window's close button (or Cmd+W). The default quits.
    fn close_requested(&mut self, cx: &mut Cx) {
        cx.quit();
    }
    /// The app was opened again while it runs (macOS: a click on its Dock icon). The
    /// default shows the window.
    fn reopen(&mut self, cx: &mut Cx) {
        cx.show_window();
    }
    /// The event loop ends (quit, or the system ends the app).
    fn exiting(&mut self) {}
    /// Repaint when the hovered hitbox changes after a frame (for views that style
    /// hover by hitbox id rather than by pointer position).
    fn repaint_on_hover_change(&self) -> bool {
        false
    }
    /// The color behind everything.
    fn background(&self) -> Color {
        Color {
            r: 1.,
            g: 1.,
            b: 1.,
            a: 1.,
        }
    }
}

/// Context passed with events.
pub struct Cx<'a> {
    redraw: bool,
    quit: bool,
    focus: &'a mut Option<Id>,
    clipboard: &'a mut Clipboard,
    wake_at: &'a mut Option<Instant>,
    window: &'a mut Option<WindowRequest>,
    capture: &'a mut Option<PathBuf>,
}

impl Cx<'_> {
    /// Request a redraw.
    pub fn notify(&mut self) {
        self.redraw = true;
    }

    pub fn quit(&mut self) {
        self.quit = true;
    }

    /// Show the window (unminimized, in front, with the keyboard).
    pub fn show_window(&mut self) {
        *self.window = Some(WindowRequest::Show);
    }

    /// Hide the window; the app keeps running.
    pub fn hide_window(&mut self) {
        *self.window = Some(WindowRequest::Hide);
    }

    /// Draw the window's content into a PNG file, for tests and bug reports. It does not
    /// wait for a frame, so it works while the window is hidden or the screen is locked.
    pub fn capture(&mut self, path: PathBuf) {
        *self.capture = Some(path);
    }

    pub fn focused(&self) -> Option<Id> {
        *self.focus
    }

    pub fn focus(&mut self, id: Option<Id>) {
        if *self.focus != id {
            *self.focus = id;
            self.redraw = true;
        }
    }

    pub fn clipboard(&mut self) -> &mut Clipboard {
        self.clipboard
    }

    /// Get an `Event::Timer` no later than `at`.
    pub fn wake_at(&mut self, at: Instant) {
        if self.wake_at.is_none_or(|w| at < w) {
            *self.wake_at = Some(at);
        }
    }
}

/// Input made up by the app itself, for automated UI tests and demos. It goes
/// through the same hit testing and dispatch as real input, without moving the
/// system's pointer or touching other windows.
#[derive(Clone, Debug)]
pub enum Synthetic {
    Move(Point),
    /// The primary button goes down at a point.
    Press(Point),
    Release(Point),
    /// Press and release at a point.
    Click(Point),
    Scroll(Point, ScrollDelta),
    Key(KeyPress),
    Ime(Ime),
    /// Committed text, as an IME commit.
    Text(String),
    /// The modifier keys held from now on (Shift for a shift-click).
    Modifiers(ModifiersState),
    /// A side button of the mouse, pressed and released.
    Nav(NavButton),
}

pub(crate) struct Host<V: View> {
    pub view: V,
    pub text: TextSystem,
    pub scene: Scene,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// The window content size in logical pixels.
    pub size: (f32, f32),
    pub dark: bool,
    pointer: Option<Point>,
    pub mods: ModifiersState,
    pub focus: Option<Id>,
    hover: Option<Id>,
    hover_layer: usize,
    pub cursor: CursorIcon,
    /// A frame must be painted.
    pub dirty: bool,
    pub wake_at: Option<Instant>,
    /// Callbacks registered while painting the frame being presented.
    after_present: Vec<FrameCallback>,
    /// Callbacks to run when the next frame begins.
    at_frame_start: Vec<FrameCallback>,
    pub clipboard: Clipboard,
    /// Where the IME goes, from the last frame; None turns the IME off.
    pub ime_area: Option<Rect>,
    pub quit: bool,
    /// What the view asked of the window, not done yet.
    pub window: Option<WindowRequest>,
    /// Where to save a capture of the window, not done yet.
    pub capture: Option<PathBuf>,
    /// Whether the window shows.
    pub visible: bool,
    last_press: Option<(Instant, Point, Option<Id>, u8)>,
}

impl<V: View> Host<V> {
    pub fn new(view: V, text: TextSystem, clipboard: Clipboard) -> Self {
        Self {
            view,
            text,
            scene: Scene::default(),
            scale: 1.,
            size: (800., 600.),
            dark: false,
            pointer: None,
            mods: ModifiersState::empty(),
            focus: None,
            hover: None,
            hover_layer: 0,
            cursor: CursorIcon::Default,
            dirty: true,
            wake_at: None,
            after_present: Vec::new(),
            at_frame_start: Vec::new(),
            clipboard,
            ime_area: None,
            quit: false,
            window: None,
            capture: None,
            visible: true,
            last_press: None,
        }
    }

    pub fn dispatch(&mut self, ev: Event<V::Message>) {
        self.with_cx(|view, cx| view.event(ev, cx));
    }

    /// Run `f` on the view with a context, and keep what it asked for.
    pub fn with_cx(&mut self, f: impl FnOnce(&mut V, &mut Cx)) {
        let mut cx = Cx {
            redraw: false,
            quit: false,
            focus: &mut self.focus,
            clipboard: &mut self.clipboard,
            wake_at: &mut self.wake_at,
            window: &mut self.window,
            capture: &mut self.capture,
        };
        f(&mut self.view, &mut cx);
        self.dirty |= cx.redraw;
        self.quit |= cx.quit;
    }

    /// The window's close button.
    pub fn close_requested(&mut self) {
        self.with_cx(|view, cx| view.close_requested(cx));
    }

    /// The app was opened again.
    pub fn reopen(&mut self) {
        self.with_cx(|view, cx| view.reopen(cx));
    }

    /// Take the window request, as done; a shown window repaints.
    pub fn take_window_request(&mut self) -> Option<WindowRequest> {
        let r = self.window.take()?;
        self.visible = r == WindowRequest::Show;
        if self.visible {
            self.dirty = true;
        }
        Some(r)
    }

    pub fn pointer_moved(&mut self, pos: Point) {
        self.pointer = Some(pos);
        self.update_hover();
        self.dispatch(Event::PointerMove { pos });
    }

    pub fn pointer_left(&mut self) {
        self.pointer = None;
        self.update_hover();
        self.dispatch(Event::PointerLeft);
    }

    pub fn button(&mut self, pressed: bool) {
        let Some(pos) = self.pointer else { return };
        let hit = self.scene.hit(pos).map(|h| h.id);
        if pressed {
            let now = Instant::now();
            let clicks = match self.last_press {
                Some((t, p, h, n))
                    if now - t < MULTI_CLICK && p.distance(pos) <= MULTI_CLICK_SLOP && h == hit =>
                {
                    n.saturating_add(1)
                }
                _ => 1,
            };
            self.last_press = Some((now, pos, hit, clicks));
            self.dispatch(Event::PointerDown {
                pos,
                hit,
                mods: self.mods,
                clicks,
            });
        } else {
            self.dispatch(Event::PointerUp { pos, hit });
        }
    }

    pub fn scroll(&mut self, delta: ScrollDelta) {
        let Some(pos) = self.pointer else { return };
        let target = self.scene.scroller(pos).map(|h| h.id);
        self.dispatch(Event::Scroll { pos, delta, target });
    }

    pub fn synthetic(&mut self, s: Synthetic) {
        match s {
            Synthetic::Move(pos) => self.pointer_moved(pos),
            Synthetic::Press(pos) => {
                self.pointer_moved(pos);
                self.button(true);
            }
            Synthetic::Release(pos) => {
                self.pointer_moved(pos);
                self.button(false);
            }
            Synthetic::Click(pos) => {
                self.pointer_moved(pos);
                self.button(true);
                self.button(false);
            }
            Synthetic::Scroll(pos, delta) => {
                self.pointer_moved(pos);
                self.scroll(delta);
            }
            Synthetic::Key(k) => self.dispatch(Event::Key(k)),
            Synthetic::Ime(ime) => self.dispatch(Event::Ime(ime)),
            Synthetic::Text(text) => self.dispatch(Event::Ime(Ime::Commit(text))),
            Synthetic::Modifiers(m) => self.mods = m,
            Synthetic::Nav(b) => self.dispatch(Event::Nav(b)),
        }
    }

    /// Run the callbacks of the frame that was presented last.
    pub fn frame_start(&mut self) {
        for f in self.at_frame_start.drain(..) {
            f();
        }
    }

    /// Paint a frame into `scene`. Afterwards `dirty` says whether another frame is
    /// needed right away (state changed while painting), and `frame_callbacks_pending`
    /// whether a frame should begin soon to run callbacks.
    pub fn paint(&mut self) {
        self.scene.clear();
        let bounds = Rect::new(0., 0., self.size.0, self.size.1);
        let mut p = Paint {
            scene: &mut self.scene,
            text: &mut self.text,
            bounds,
            pointer: self.pointer,
            hover: self.hover,
            hover_layer: self.hover_layer,
            focus: &mut self.focus,
            clip: bounds,
            layer: 0,
            next_frame: &mut self.after_present,
            ime_area: None,
            clipboard: &mut self.clipboard,
            repaint: false,
            quit: false,
            wake_at: &mut self.wake_at,
            scale: self.scale,
            dark: self.dark,
            now: Instant::now(),
        };
        self.view.paint(&mut p);
        let (ime_area, repaint, quit) = (p.ime_area, p.repaint, p.quit);
        self.dirty = false;
        self.text.end_frame();
        self.ime_area = ime_area;
        // Content moved under a still pointer (scrolling, new layers): keep hover in sync.
        let hover_changed = self.sync_hover();
        self.at_frame_start.append(&mut self.after_present);
        if repaint || (hover_changed && self.view.repaint_on_hover_change()) {
            self.dirty = true;
        }
        self.quit |= quit;
    }

    pub fn frame_callbacks_pending(&self) -> bool {
        !self.at_frame_start.is_empty()
    }

    /// Redraw when the topmost hitbox under the pointer changes.
    fn update_hover(&mut self) {
        if self.sync_hover() {
            self.dirty = true;
        }
    }

    /// Update the hovered hitbox and the cursor; true if the hovered hitbox changed.
    fn sync_hover(&mut self) -> bool {
        let hit = self.pointer.and_then(|p| self.scene.hit(p));
        self.hover_layer = hit.map_or(0, |h| h.layer);
        let (id, cursor) = hit.map_or((None, CursorIcon::Default), |h| (Some(h.id), h.cursor));
        self.cursor = cursor;
        let changed = id != self.hover;
        self.hover = id;
        changed
    }
}
