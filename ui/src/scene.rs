//! The per-frame display list: quads, text, icons and hitboxes, each in a layer.
//!
//! Higher layers draw on top and win hit tests (popups, dialogs, toasts). Within a
//! layer, what was added later is on top. The hitboxes of the last frame drive
//! clicks, hover tracking and the mouse cursor.

use std::hash::{Hash, Hasher};

use winit::window::CursorIcon;

use crate::geom::{Color, Point, Rect};
use crate::image::Image;
use crate::text::TextLayout;

/// Identifies an interactive region or a widget's state. Views pick the values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(pub u64);

impl Id {
    /// An id from any hashable key, e.g. `Id::of(("row", 3))`.
    pub fn of(key: impl Hash) -> Id {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        Id(h.finish())
    }

    /// A child id.
    pub fn with(self, key: impl Hash) -> Id {
        Id::of((self.0, key))
    }
}

/// Layers, bottom to top.
pub mod layer {
    pub const BASE: usize = 0;
    pub const POPUP: usize = 1;
    pub const MODAL: usize = 2;
    pub const MODAL_POPUP: usize = 3;
    pub const TOP: usize = 4;
    pub const COUNT: usize = 5;
}

#[derive(Clone, Copy, Debug)]
pub struct Quad {
    pub rect: Rect,
    pub clip: Rect,
    pub color: Color,
    pub radius: f32,
    pub border: f32,
    pub border_color: Color,
    /// Blur radius for soft shadows; 0 for crisp edges.
    pub blur: f32,
}

pub struct TextDraw {
    pub layout: TextLayout,
    pub x: f32,
    pub y: f32,
    pub clip: Rect,
    pub color: Color,
}

/// An icon from the registry (see `icons`), drawn as a square.
pub struct IconDraw {
    pub icon: u16,
    pub rect: Rect,
    pub clip: Rect,
    pub color: Color,
}

/// A registered bitmap image, scaled to `rect`.
pub struct ImageDraw {
    pub image: Image,
    pub rect: Rect,
    pub clip: Rect,
    /// Draw the image's alpha in this color (monochrome icons).
    pub tint: Option<Color>,
}

#[derive(Clone, Copy, Debug)]
pub struct Hitbox {
    pub rect: Rect,
    pub id: Id,
    pub cursor: CursorIcon,
    pub layer: usize,
}

#[derive(Default)]
pub struct Layer {
    /// Drawn first.
    pub quads: Vec<Quad>,
    /// Drawn over the quads.
    pub images: Vec<ImageDraw>,
    /// Drawn over the images.
    pub texts: Vec<TextDraw>,
    pub icons: Vec<IconDraw>,
    /// Drawn over the text (carets, focus rings).
    pub overlay: Vec<Quad>,
}

#[derive(Default)]
pub struct Scene {
    pub layers: [Layer; layer::COUNT],
    pub hitboxes: Vec<Hitbox>,
    /// Scroll targets, with the same order rules as hitboxes.
    pub scrollers: Vec<Hitbox>,
}

impl Scene {
    pub fn clear(&mut self) {
        for l in &mut self.layers {
            l.quads.clear();
            l.images.clear();
            l.texts.clear();
            l.icons.clear();
            l.overlay.clear();
        }
        self.hitboxes.clear();
        self.scrollers.clear();
    }

    /// Topmost hitbox under a point.
    pub fn hit(&self, p: Point) -> Option<&Hitbox> {
        topmost(&self.hitboxes, p)
    }

    /// Topmost scroll region under a point.
    pub fn scroller(&self, p: Point) -> Option<&Hitbox> {
        topmost(&self.scrollers, p)
    }

    /// The rect of a hitbox drawn this frame.
    pub fn hitbox_rect(&self, id: Id) -> Option<Rect> {
        self.hitboxes
            .iter()
            .rev()
            .find(|h| h.id == id)
            .map(|h| h.rect)
    }

    pub fn quad_count(&self) -> usize {
        self.layers
            .iter()
            .map(|l| l.quads.len() + l.overlay.len())
            .sum()
    }

    pub fn image_count(&self) -> usize {
        self.layers.iter().map(|l| l.images.len()).sum()
    }

    pub fn text_count(&self) -> usize {
        self.layers.iter().map(|l| l.texts.len()).sum()
    }
}

fn topmost(list: &[Hitbox], p: Point) -> Option<&Hitbox> {
    let mut best: Option<&Hitbox> = None;
    for h in list.iter().filter(|h| h.rect.contains(p)) {
        if best.is_none_or(|b| h.layer >= b.layer) {
            best = Some(h);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hitbox(scene: &mut Scene, rect: Rect, id: u64, layer: usize) {
        scene.hitboxes.push(Hitbox {
            rect,
            id: Id(id),
            cursor: CursorIcon::Default,
            layer,
        });
    }

    #[test]
    fn later_hitboxes_of_a_layer_are_on_top() {
        let mut s = Scene::default();
        hitbox(&mut s, Rect::new(0., 0., 100., 100.), 1, layer::BASE);
        hitbox(&mut s, Rect::new(10., 10., 20., 20.), 2, layer::BASE);
        assert_eq!(s.hit(Point::new(15., 15.)).map(|h| h.id), Some(Id(2)));
        assert_eq!(s.hit(Point::new(50., 50.)).map(|h| h.id), Some(Id(1)));
        assert!(s.hit(Point::new(150., 50.)).is_none());
    }

    #[test]
    fn higher_layers_win_whatever_the_order() {
        let mut s = Scene::default();
        hitbox(&mut s, Rect::new(0., 0., 100., 100.), 1, layer::MODAL);
        hitbox(&mut s, Rect::new(0., 0., 50., 50.), 2, layer::BASE);
        assert_eq!(s.hit(Point::new(10., 10.)).map(|h| h.id), Some(Id(1)));
    }

    #[test]
    fn ids_are_stable_and_children_differ() {
        assert_eq!(Id::of(("row", 3)), Id::of(("row", 3)));
        assert_ne!(Id::of("a").with(1), Id::of("a").with(2));
    }
}
