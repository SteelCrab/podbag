//! The window and the event loop (winit), and the GPU rendering of each frame.
//!
//! Frames are painted only on demand (`ControlFlow::Wait`): after an event the view
//! marks with `Cx::notify`, when the hovered hitbox changes, on resize and scale
//! changes, for timers the view asked for, and for frame callbacks.

use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorIcon, Icon, Theme, Window, WindowId, WindowLevel};

use crate::geom::{Point, Rect};
use crate::gpu::Gpu;
use crate::host::{Event, Host, NavButton, Synthetic, View, WindowRequest};
use crate::image::ImageData;
use crate::input::{Clipboard, KeyPress, ScrollDelta};
use crate::scene::Id;
use crate::text::{Fonts, TextSystem, load_fonts};

static START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the app started (debug logs).
fn since_start() -> f64 {
    START.get().map_or(0., |s| s.elapsed().as_secs_f64() * 1e3)
}

pub struct WindowOptions {
    pub title: String,
    /// Logical size.
    pub width: f64,
    pub height: f64,
    pub min_size: Option<(f64, f64)>,
    /// The widget focused at start.
    pub focus: Option<Id>,
    /// Timing and frame logs on stderr.
    pub debug: bool,
    /// Bring the window to the front and give it the keyboard when it opens.
    pub activate: bool,
    /// Keep the window above the others.
    pub always_on_top: bool,
    /// The window's icon (title bar and taskbar on Windows and Linux; macOS takes the
    /// app bundle's).
    pub icon: Option<ImageData>,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: String::new(),
            width: 1024.,
            height: 768.,
            min_size: None,
            focus: None,
            debug: false,
            activate: true,
            always_on_top: false,
            icon: None,
        }
    }
}

enum UserEvent<M> {
    Message(M),
    Synthetic(Synthetic),
    /// macOS: the Dock icon was clicked.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Reopen,
}

/// Sends messages (and synthetic input) to the view from any thread.
pub struct Proxy<M: 'static>(EventLoopProxy<UserEvent<M>>);

impl<M: 'static> Clone for Proxy<M> {
    fn clone(&self) -> Self {
        Proxy(self.0.clone())
    }
}

impl<M: 'static> Proxy<M> {
    /// Send a message to the view (wakes the loop). False once the loop is gone.
    pub fn send(&self, m: M) -> bool {
        self.0.send_event(UserEvent::Message(m)).is_ok()
    }

    /// Feed synthetic input, for automated UI tests and demos.
    pub fn synthetic(&self, s: Synthetic) -> bool {
        self.0.send_event(UserEvent::Synthetic(s)).is_ok()
    }
}

pub struct App<M: 'static> {
    event_loop: EventLoop<UserEvent<M>>,
    fonts: JoinHandle<Fonts>,
}

impl<M: Send + 'static> App<M> {
    /// Starts loading fonts on a thread right away, in parallel with window and GPU setup.
    pub fn new() -> Self {
        START.get_or_init(Instant::now);
        let fonts = std::thread::spawn(load_fonts);
        let event_loop = EventLoop::with_user_event().build().expect("event loop");
        #[cfg(target_os = "macos")]
        {
            let proxy = event_loop.create_proxy();
            crate::macos::on_reopen(move || {
                let _ = proxy.send_event(UserEvent::Reopen);
            });
        }
        Self { event_loop, fonts }
    }

    pub fn proxy(&self) -> Proxy<M> {
        Proxy(self.event_loop.create_proxy())
    }

    /// Open the window and run until it closes. `build` makes the view once the
    /// window exists.
    pub fn run<V: View<Message = M>>(self, opts: WindowOptions, build: impl FnOnce() -> V) {
        self.event_loop.set_control_flow(ControlFlow::Wait);
        let mut runner = Runner {
            opts,
            build: Some(build),
            fonts: Some(self.fonts),
            live: None,
        };
        self.event_loop.run_app(&mut runner).expect("event loop");
    }
}

impl<M: Send + 'static> Default for App<M> {
    fn default() -> Self {
        Self::new()
    }
}

struct Runner<V: View, B> {
    opts: WindowOptions,
    build: Option<B>,
    fonts: Option<JoinHandle<Fonts>>,
    live: Option<Live<V>>,
}

struct Live<V: View> {
    host: Host<V>,
    gpu: Gpu,
    cursor: CursorIcon,
    ime_allowed: bool,
    ime_area: Option<Rect>,
    debug: bool,
    // Last, so it is dropped after the GPU surface.
    window: Arc<Window>,
}

impl<V: View, B: FnOnce() -> V> ApplicationHandler<UserEvent<V::Message>> for Runner<V, B> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.live.is_some() {
            return;
        }
        let t = Instant::now();
        let o = &self.opts;
        let mut attrs = Window::default_attributes()
            .with_title(o.title.clone())
            .with_inner_size(LogicalSize::new(o.width, o.height))
            .with_active(o.activate);
        if o.always_on_top {
            attrs = attrs.with_window_level(WindowLevel::AlwaysOnTop);
        }
        if let Some(icon) = &o.icon {
            match Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height) {
                Ok(icon) => attrs = attrs.with_window_icon(Some(icon)),
                Err(e) => eprintln!("window icon: {e}"),
            }
        }
        if let Some((w, h)) = o.min_size {
            attrs = attrs.with_min_inner_size(LogicalSize::new(w, h));
        }
        if let Some(m) = el.primary_monitor() {
            let s = m.scale_factor();
            let (mw, mh) = (m.size().width as f64 / s, m.size().height as f64 / s);
            let (mx, my) = (m.position().x as f64 / s, m.position().y as f64 / s);
            attrs = attrs.with_position(LogicalPosition::new(
                mx + (mw - o.width) / 2.,
                my + (mh - o.height) / 2.,
            ));
        }
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let t_window = t.elapsed();
        let gpu = Gpu::new(window.clone(), el.owned_display_handle());
        let t_gpu = t.elapsed();
        let fonts = self
            .fonts
            .take()
            .expect("fonts")
            .join()
            .expect("font thread");
        let debug = o.debug;
        if debug {
            eprintln!(
                "{:.1} ms: window {:.1} ms, gpu {:.1} ms ({}), fonts {:.1} ms: {} faces, {:?}, {}; waited {:.1} ms",
                since_start(),
                t_window.as_secs_f64() * 1e3,
                (t_gpu - t_window).as_secs_f64() * 1e3,
                gpu.adapter_info,
                fonts.load_ms,
                fonts.faces,
                fonts.family,
                fonts.strategy,
                (t.elapsed() - t_gpu).as_secs_f64() * 1e3,
            );
        }
        let view = (self.build.take().expect("view"))();
        let mut host = Host::new(view, TextSystem::new(fonts), Clipboard::default());
        host.focus = o.focus;
        host.dark = window.theme() == Some(Theme::Dark);
        host.scale = window.scale_factor() as f32;
        let size = window.inner_size();
        host.size = (
            size.width as f32 / host.scale,
            size.height as f32 / host.scale,
        );
        window.set_ime_allowed(false);
        window.request_redraw();
        self.live = Some(Live {
            host,
            gpu,
            cursor: CursorIcon::Default,
            ime_allowed: false,
            ime_area: None,
            debug,
            window,
        });
    }

    fn new_events(&mut self, el: &ActiveEventLoop, cause: StartCause) {
        if let (StartCause::ResumeTimeReached { .. }, Some(live)) = (cause, &mut self.live)
            && live.host.wake_at.is_some_and(|w| w <= Instant::now())
        {
            live.host.wake_at = None;
            live.host.dispatch(Event::Timer);
            live.after_input(el);
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        if let Some(live) = &mut self.live {
            live.host.view.exiting();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let flow = match self.live.as_ref().and_then(|l| l.host.wake_at) {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        };
        el.set_control_flow(flow);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent<V::Message>) {
        let Some(live) = &mut self.live else { return };
        match ev {
            UserEvent::Message(m) => live.host.dispatch(Event::Message(m)),
            UserEvent::Synthetic(s) => live.host.synthetic(s),
            UserEvent::Reopen => live.host.reopen(),
        }
        live.after_input(el);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(live) = &mut self.live else { return };
        let host = &mut live.host;
        match event {
            WindowEvent::CloseRequested => host.close_requested(),
            WindowEvent::Resized(size) => {
                live.gpu.resize(size.width, size.height);
                host.size = (
                    size.width as f32 / host.scale,
                    size.height as f32 / host.scale,
                );
                host.dirty = true;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Moved to a display with another scale: same logical size, new pixels.
                // A `Resized` with the new physical size follows.
                host.scale = scale_factor as f32;
                let size = live.window.inner_size();
                live.gpu.resize(size.width, size.height);
                host.size = (
                    size.width as f32 / host.scale,
                    size.height as f32 / host.scale,
                );
                host.dirty = true;
            }
            WindowEvent::ThemeChanged(theme) => {
                host.dark = theme == Theme::Dark;
                host.dirty = true;
            }
            WindowEvent::Occluded(false) if host.dirty => live.window.request_redraw(),
            WindowEvent::RedrawRequested => {
                // `frame` asks for the next redraw itself; an occluded window must not
                // spin on redraw requests until it shows again.
                live.frame();
                if live.host.quit {
                    el.exit();
                }
                return;
            }
            WindowEvent::ModifiersChanged(m) => host.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                host.pointer_moved(Point::new(
                    position.x as f32 / host.scale,
                    position.y as f32 / host.scale,
                ));
            }
            WindowEvent::CursorLeft { .. } => host.pointer_left(),
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                host.button(state == ElementState::Pressed);
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: button @ (MouseButton::Back | MouseButton::Forward),
                ..
            } => {
                let nav = if button == MouseButton::Back {
                    NavButton::Back
                } else {
                    NavButton::Forward
                };
                host.dispatch(Event::Nav(nav));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let delta = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines(x, y),
                    MouseScrollDelta::PixelDelta(p) => {
                        ScrollDelta::Pixels(p.x as f32 / host.scale, p.y as f32 / host.scale)
                    }
                };
                host.scroll(delta);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let key = KeyPress::from_winit(&event, host.mods);
                host.dispatch(Event::Key(key));
            }
            WindowEvent::Ime(ime) => host.dispatch(Event::Ime(ime)),
            _ => {}
        }
        live.after_input(el);
    }
}

impl<V: View> Live<V> {
    /// Apply what the host asked for: the window shown or hidden, a redraw, the
    /// cursor, quitting.
    fn after_input(&mut self, el: &ActiveEventLoop) {
        match self.host.take_window_request() {
            Some(WindowRequest::Show) => {
                self.window.set_visible(true);
                self.window.set_minimized(false);
                self.window.focus_window();
            }
            Some(WindowRequest::Hide) => self.window.set_visible(false),
            None => {}
        }
        if let Some(path) = self.host.capture.take()
            && let Err(e) = self.capture(&path)
        {
            eprintln!("capture {}: {e}", path.display());
        }
        if self.host.dirty {
            self.window.request_redraw();
        }
        if self.host.cursor != self.cursor {
            self.cursor = self.host.cursor;
            self.window.set_cursor(self.cursor);
        }
        if self.host.quit {
            el.exit();
        }
    }

    /// Paint now and save the window's content as PNG, without presenting it.
    fn capture(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        // Paint until the layout settles: some sizes come from the previous frame.
        for _ in 0..4 {
            self.host.paint();
            if !self.host.dirty {
                break;
            }
        }
        let clear = self.host.view.background();
        let host = &mut self.host;
        let rgba = self
            .gpu
            .capture(&host.scene, &mut host.text.fonts, host.scale, clear);
        let (w, h) = self.gpu.size();
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut png = png::Encoder::new(file, w, h);
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        png.write_header()
            .and_then(|mut out| out.write_image_data(&rgba))
            .map_err(std::io::Error::other)
    }

    fn frame(&mut self) {
        if self.debug && self.host.frame_callbacks_pending() {
            eprintln!("{:.1} ms: frame callbacks", since_start());
        }
        self.host.frame_start();
        // A hidden window paints when it shows again.
        if !self.host.dirty || !self.host.visible {
            return;
        }
        // Occluded windows get no surface texture (wgpu on macOS); Occluded(false) retries.
        let target = match self.gpu.acquire() {
            Ok(t) => t,
            Err(retry) => {
                if self.debug {
                    eprintln!(
                        "{:.1} ms: no surface texture (retry {retry})",
                        since_start()
                    );
                }
                if retry {
                    self.window.request_redraw();
                }
                return;
            }
        };
        let t = Instant::now();
        self.host.paint();
        let t_paint = t.elapsed();
        let clear = self.host.view.background();
        let host = &mut self.host;
        self.gpu.render(
            target,
            &host.scene,
            &mut host.text.fonts,
            host.scale,
            clear,
            &self.window,
        );
        let allow = host.ime_area.is_some();
        if allow != self.ime_allowed {
            self.ime_allowed = allow;
            self.window.set_ime_allowed(allow);
        }
        if host.ime_area != self.ime_area {
            self.ime_area = host.ime_area;
            if let Some(r) = host.ime_area {
                self.window.set_ime_cursor_area(
                    LogicalPosition::new(r.x, r.y),
                    LogicalSize::new(r.w, r.h),
                );
            }
        }
        if host.cursor != self.cursor {
            self.cursor = host.cursor;
            self.window.set_cursor(self.cursor);
        }
        if host.dirty || host.frame_callbacks_pending() {
            self.window.request_redraw();
        }
        if self.debug {
            eprintln!(
                "{:.1} ms: frame: paint {:.2} ms, render {:.2} ms, {} quads, {} texts",
                since_start(),
                t_paint.as_secs_f64() * 1e3,
                (t.elapsed() - t_paint).as_secs_f64() * 1e3,
                host.scene.quad_count(),
                host.scene.text_count(),
            );
        }
    }
}
