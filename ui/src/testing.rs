//! Run a view without a window or a GPU, for automated UI tests.
//!
//! A `Harness` feeds `Synthetic` input through the same dispatch, hit testing,
//! focus and frame logic as a real window, and paints frames into a `Scene` that
//! tests can inspect. The clipboard is in memory. Text shapes with no fonts unless
//! the harness is given some (`with_fonts`), which is enough for tests that do not
//! measure text.
//!
//! ```no_run
//! use podbag_ui::testing::Harness;
//! use podbag_ui::ui::UiView;
//! # struct MyApp;
//! # impl podbag_ui::ui::UiApp for MyApp {
//! #     type Message = ();
//! #     fn ui(&mut self, _: &mut podbag_ui::ui::Ui) {}
//! #     fn message(&mut self, _: (), _: &mut podbag_ui::Cx) {}
//! #     fn theme(&self, _: bool) -> podbag_ui::ui::Theme { Default::default() }
//! # }
//! let mut h = Harness::new(UiView::new(MyApp), 800., 600.);
//! h.tab();
//! h.press_key(podbag_ui::KeyPress::named(podbag_ui::NamedKey::Enter));
//! ```

use winit::keyboard::{ModifiersState, NamedKey};

use crate::geom::{Point, Rect};
use crate::gpu::Gpu;
use crate::host::{Event, Host, NavButton, Synthetic, View};
use crate::input::{Clipboard, KeyPress};
use crate::scene::{Id, Scene};
use crate::text::{Fonts, TextSystem};

pub struct Harness<V: View> {
    host: Host<V>,
}

impl<V: View> Harness<V> {
    /// A `width` x `height` (logical) window without fonts. Paints the first frame.
    pub fn new(view: V, width: f32, height: f32) -> Self {
        Self::with_fonts(view, width, height, Fonts::empty())
    }

    pub fn with_fonts(view: V, width: f32, height: f32, fonts: Fonts) -> Self {
        let mut host = Host::new(view, TextSystem::new(fonts), Clipboard::in_memory());
        host.size = (width, height);
        let mut h = Self { host };
        h.settle();
        h
    }

    /// Feed one input event, then paint until the view is idle.
    pub fn input(&mut self, s: Synthetic) {
        self.host.synthetic(s);
        self.apply_window();
        self.settle();
    }

    /// Send a message, then paint until idle.
    pub fn message(&mut self, m: V::Message) {
        self.host.dispatch(Event::Message(m));
        self.apply_window();
        self.settle();
    }

    /// Fire the timer the view asked for (whether or not its time came).
    pub fn timer(&mut self) {
        self.host.wake_at = None;
        self.host.dispatch(Event::Timer);
        self.settle();
    }

    pub fn press_key(&mut self, key: KeyPress) {
        self.input(Synthetic::Key(key));
    }

    /// A mouse side button (4 or 5), pressed and released.
    pub fn nav(&mut self, button: NavButton) {
        self.input(Synthetic::Nav(button));
    }

    /// The window's close button.
    pub fn close_window(&mut self) {
        self.host.close_requested();
        self.apply_window();
        self.settle();
    }

    /// The app is opened again (a click on the Dock icon).
    pub fn reopen(&mut self) {
        self.host.reopen();
        self.apply_window();
        self.settle();
    }

    /// Whether the window shows (after what the view asked).
    pub fn window_visible(&self) -> bool {
        self.host.visible
    }

    fn apply_window(&mut self) {
        self.host.take_window_request();
    }

    pub fn tab(&mut self) {
        self.press_key(KeyPress::named(NamedKey::Tab));
    }

    pub fn shift_tab(&mut self) {
        self.press_key(KeyPress::named(NamedKey::Tab).with_mods(ModifiersState::SHIFT));
    }

    pub fn click(&mut self, at: Point) {
        self.input(Synthetic::Click(at));
    }

    /// Click the middle of the hitbox `id` painted last. Panics if there is none.
    pub fn click_id(&mut self, id: Id) {
        let r = self
            .hitbox(id)
            .unwrap_or_else(|| panic!("no hitbox {id:?}"));
        self.click(Point::new(r.x + r.w / 2., r.y + r.h / 2.));
    }

    /// Press at `from`, move to `to` and release.
    pub fn drag(&mut self, from: Point, to: Point) {
        self.input(Synthetic::Press(from));
        self.input(Synthetic::Move(to));
        self.input(Synthetic::Release(to));
    }

    /// Paint frames until none is needed (at most 10), running frame callbacks.
    pub fn settle(&mut self) {
        for _ in 0..10 {
            self.host.frame_start();
            if !self.host.dirty {
                break;
            }
            self.host.paint();
        }
    }

    /// Paint one frame even if nothing changed.
    pub fn repaint(&mut self) {
        self.host.dirty = true;
        self.settle();
    }

    pub fn resize(&mut self, width: f32, height: f32) {
        self.host.size = (width, height);
        self.repaint();
    }

    pub fn set_dark(&mut self, dark: bool) {
        self.host.dark = dark;
        self.repaint();
    }

    pub fn view(&self) -> &V {
        &self.host.view
    }

    pub fn view_mut(&mut self) -> &mut V {
        &mut self.host.view
    }

    pub fn scene(&self) -> &Scene {
        &self.host.scene
    }

    /// The texts drawn in the last frame, in paint order.
    pub fn texts(&self) -> Vec<String> {
        let layers = self.host.scene.layers.iter();
        layers
            .flat_map(|l| l.texts.iter().map(|t| t.layout.text().to_string()))
            .collect()
    }

    /// Whether the last frame drew a text that contains `part`.
    pub fn shows(&self, part: &str) -> bool {
        self.texts().iter().any(|t| t.contains(part))
    }

    pub fn hitbox(&self, id: Id) -> Option<Rect> {
        self.host.scene.hitbox_rect(id)
    }

    pub fn focused(&self) -> Option<Id> {
        self.host.focus
    }

    /// Whether the last frame turned the IME on (a text field has the focus).
    pub fn ime_enabled(&self) -> bool {
        self.host.ime_area.is_some()
    }

    pub fn clipboard(&mut self) -> &mut Clipboard {
        &mut self.host.clipboard
    }

    /// The time the view asked to be woken at, if any.
    pub fn wake_at(&self) -> Option<std::time::Instant> {
        self.host.wake_at
    }

    pub fn quit_requested(&self) -> bool {
        self.host.quit
    }

    /// Paint a frame at `scale` pixels per point and draw it with the GPU, without a
    /// window (screenshots and previews): (width, height, RGBA rows). None without a GPU.
    pub fn capture(&mut self, scale: f32) -> Option<(u32, u32, Vec<u8>)> {
        self.host.scale = scale;
        self.repaint();
        let (w, h) = (
            (self.host.size.0 * scale).round() as u32,
            (self.host.size.1 * scale).round() as u32,
        );
        let mut gpu = Gpu::headless(w, h)?;
        let clear = self.host.view.background();
        let rgba = gpu.capture(&self.host.scene, &mut self.host.text.fonts, scale, clear);
        Some((w, h, rgba))
    }
}
