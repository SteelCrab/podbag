//! The paint context: what a frame draws into, plus the window state a frame may
//! read (pointer, hover, focus, scale, appearance) or change (focus, clipboard,
//! repaint and wake-up requests).

use std::time::Instant;

use winit::window::CursorIcon;

use crate::geom::{Color, Point, Rect};
use crate::image::Image;
use crate::input::Clipboard;
use crate::scene::{Hitbox, IconDraw, Id, ImageDraw, Quad, Scene, TextDraw, layer};
use crate::text::{TextLayout, TextStyle, TextSystem};

pub type FrameCallback = Box<dyn FnOnce()>;

pub struct Paint<'a> {
    pub(crate) scene: &'a mut Scene,
    pub text: &'a mut TextSystem,
    pub(crate) bounds: Rect,
    pub(crate) pointer: Option<Point>,
    pub(crate) hover: Option<Id>,
    pub(crate) hover_layer: usize,
    pub(crate) focus: &'a mut Option<Id>,
    pub(crate) clip: Rect,
    pub(crate) layer: usize,
    pub(crate) next_frame: &'a mut Vec<FrameCallback>,
    pub(crate) ime_area: Option<Rect>,
    pub(crate) clipboard: &'a mut Clipboard,
    pub(crate) repaint: bool,
    pub(crate) quit: bool,
    pub(crate) wake_at: &'a mut Option<Instant>,
    pub(crate) scale: f32,
    pub(crate) dark: bool,
    pub(crate) now: Instant,
}

impl Paint<'_> {
    /// The window content area.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Physical pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// True if the system appearance is dark.
    pub fn system_dark(&self) -> bool {
        self.dark
    }

    /// When this frame started. Use it for timing (tooltips, animations) so a frame
    /// sees one time.
    pub fn now(&self) -> Instant {
        self.now
    }

    pub fn pointer(&self) -> Option<Point> {
        self.pointer
    }

    pub fn is_focused(&self, id: Id) -> bool {
        *self.focus == Some(id)
    }

    pub fn focused(&self) -> Option<Id> {
        *self.focus
    }

    /// Move the keyboard focus. The IME is on while a focused text field paints.
    pub fn set_focus(&mut self, id: Option<Id>) {
        *self.focus = id;
    }

    pub fn clipboard(&mut self) -> &mut Clipboard {
        self.clipboard
    }

    /// Paint again after this frame (state changed while painting).
    pub fn request_repaint(&mut self) {
        self.repaint = true;
    }

    /// Close the window and end the event loop after this frame.
    pub fn quit(&mut self) {
        self.quit = true;
    }

    /// Paint again no later than `at`.
    pub fn wake_at(&mut self, at: Instant) {
        if self.wake_at.is_none_or(|w| at < w) {
            *self.wake_at = Some(at);
        }
    }

    /// True if the pointer is over `rect` and inside the current clip.
    pub fn hovered(&self, rect: Rect) -> bool {
        self.pointer
            .is_some_and(|p| rect.contains(p) && self.clip.contains(p))
    }

    /// True if the pointer is over `rect` and nothing of a higher layer covers it.
    pub fn hovered_area(&self, rect: Rect) -> bool {
        self.hovered(rect) && (self.hover.is_none() || self.hover_layer <= self.layer)
    }

    /// True if `id` is the topmost hitbox under the pointer (as of the last frame).
    pub fn is_hovered(&self, id: Id) -> bool {
        self.hover == Some(id)
    }

    pub fn fill(&mut self, rect: Rect, color: Color) {
        self.rounded(rect, 0., color);
    }

    pub fn rounded(&mut self, rect: Rect, radius: f32, color: Color) {
        self.bordered(rect, radius, color, 0., Color::TRANSPARENT);
    }

    pub fn bordered(
        &mut self,
        rect: Rect,
        radius: f32,
        fill: Color,
        border: f32,
        border_color: Color,
    ) {
        let quad = Quad {
            rect,
            clip: self.clip,
            color: fill,
            radius,
            border,
            border_color,
            blur: 0.,
        };
        self.scene.layers[self.layer].quads.push(quad);
    }

    /// A soft shadow under a rounded rect.
    pub fn shadow(&mut self, rect: Rect, radius: f32, blur: f32, color: Color) {
        let r = Rect::new(
            rect.x - blur,
            rect.y - blur + blur / 3.,
            rect.w + blur * 2.,
            rect.h + blur * 2.,
        );
        let quad = Quad {
            rect: r,
            clip: self.clip,
            color,
            radius: radius + blur,
            border: 0.,
            border_color: color,
            blur,
        };
        self.scene.layers[self.layer].quads.push(quad);
    }

    /// A quad drawn above all text of the layer, e.g. a caret.
    pub fn overlay(&mut self, rect: Rect, color: Color) {
        let quad = Quad {
            rect,
            clip: self.clip,
            color,
            radius: 0.,
            border: 0.,
            border_color: color,
            blur: 0.,
        };
        self.scene.layers[self.layer].overlay.push(quad);
    }

    /// An outline above the text of `layer` (focus rings).
    pub(crate) fn ring(
        &mut self,
        layer: usize,
        rect: Rect,
        radius: f32,
        width: f32,
        color: Color,
        clip: Rect,
    ) {
        let quad = Quad {
            rect,
            clip,
            color: Color::TRANSPARENT,
            radius,
            border: width,
            border_color: color,
            blur: 0.,
        };
        self.scene.layers[layer.min(layer::COUNT - 1)]
            .overlay
            .push(quad);
    }

    /// One-pixel line along the bottom edge.
    pub fn border_bottom(&mut self, rect: Rect, color: Color) {
        self.fill(Rect::new(rect.x, rect.bottom() - 1., rect.w, 1.), color);
    }

    pub fn layout(&mut self, text: &str, style: TextStyle, max_width: Option<f32>) -> TextLayout {
        self.text.layout(text, style, max_width)
    }

    /// Text wrapped at `width`, any number of lines.
    pub fn layout_wrapped(&mut self, text: &str, style: TextStyle, width: f32) -> TextLayout {
        self.text.layout_wrapped(text, style, width)
    }

    /// Draw a laid-out text with its first line box top-left at (x, y).
    pub fn draw_text(&mut self, layout: &TextLayout, x: f32, y: f32, color: Color) {
        let clip = self.clip;
        self.scene.layers[self.layer].texts.push(TextDraw {
            layout: layout.clone(),
            x,
            y,
            clip,
            color,
        });
    }

    /// Left-aligned, vertically centered, clipped text with an ellipsis if it is too wide.
    pub fn label(&mut self, rect: Rect, text: &str, style: TextStyle, color: Color) -> TextLayout {
        let layout = self.layout(text, style, Some(rect.w));
        let y = rect.y + (rect.h - layout.height) / 2.;
        self.clip(rect, |p| p.draw_text(&layout, rect.x, y, color));
        layout
    }

    /// An icon from the registry, filling `rect` (square).
    pub fn icon(&mut self, icon: u16, rect: Rect, color: Color) {
        let clip = self.clip;
        self.scene.layers[self.layer].icons.push(IconDraw {
            icon,
            rect,
            clip,
            color,
        });
    }

    /// A bitmap image, scaled to fill `rect`.
    pub fn image(&mut self, image: Image, rect: Rect) {
        self.push_image(image, rect, None);
    }

    /// A bitmap image's alpha in one color (a monochrome icon).
    pub fn image_tinted(&mut self, image: Image, rect: Rect, color: Color) {
        self.push_image(image, rect, Some(color));
    }

    fn push_image(&mut self, image: Image, rect: Rect, tint: Option<Color>) {
        let clip = self.clip;
        self.scene.layers[self.layer].images.push(ImageDraw {
            image,
            rect,
            clip,
            tint,
        });
    }

    /// A clickable region for this frame's hit tests (clipped).
    pub fn hitbox(&mut self, rect: Rect, id: Id, cursor: CursorIcon) {
        let rect = rect.intersect(&self.clip);
        self.scene.hitboxes.push(Hitbox {
            rect,
            id,
            cursor,
            layer: self.layer,
        });
    }

    /// A region that takes wheel and trackpad scrolling.
    pub fn scroll_region(&mut self, rect: Rect, id: Id) {
        let rect = rect.intersect(&self.clip);
        let cursor = CursorIcon::Default;
        self.scene.scrollers.push(Hitbox {
            rect,
            id,
            cursor,
            layer: self.layer,
        });
    }

    /// Paint with an extra clip rect.
    pub fn clip<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.clip;
        self.clip = old.intersect(&rect);
        let r = f(self);
        self.clip = old;
        r
    }

    pub fn current_clip(&self) -> Rect {
        self.clip
    }

    pub fn current_layer(&self) -> usize {
        self.layer
    }

    /// Paint into another layer; the clip is reset to the window.
    pub fn on_layer<R>(&mut self, layer: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        let (old_layer, old_clip) = (self.layer, self.clip);
        self.layer = layer.min(layer::COUNT - 1);
        self.clip = self.bounds;
        let r = f(self);
        self.layer = old_layer;
        self.clip = old_clip;
        r
    }

    /// Run `f` when the frame after this one begins, i.e. once this frame was presented.
    pub fn on_next_frame(&mut self, f: impl FnOnce() + 'static) {
        self.next_frame.push(Box::new(f));
    }

    /// Where the IME candidate window should go (the focused text field's caret).
    /// A frame that sets it turns the IME on.
    pub fn set_ime_area(&mut self, rect: Rect) {
        self.ime_area = Some(rect);
    }
}
