//! Geometry and color. All positions are logical pixels unless noted.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn distance(self, o: Point) -> f32 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0.), (b - y).max(0.))
    }

    /// Shrink by padding on each side (left, top, right, bottom).
    pub fn inset(&self, l: f32, t: f32, r: f32, b: f32) -> Rect {
        Rect::new(
            self.x + l,
            self.y + t,
            (self.w - l - r).max(0.),
            (self.h - t - b).max(0.),
        )
    }

    pub fn pad(&self, px: f32, py: f32) -> Rect {
        self.inset(px, py, px, py)
    }

    /// Rect-cut layout: take a slice off one side and shrink `self`.
    pub fn cut_left(&mut self, w: f32) -> Rect {
        let w = w.min(self.w);
        let out = Rect::new(self.x, self.y, w, self.h);
        self.x += w;
        self.w -= w;
        out
    }

    pub fn cut_right(&mut self, w: f32) -> Rect {
        let w = w.min(self.w);
        self.w -= w;
        Rect::new(self.x + self.w, self.y, w, self.h)
    }

    pub fn cut_top(&mut self, h: f32) -> Rect {
        let h = h.min(self.h);
        let out = Rect::new(self.x, self.y, self.w, h);
        self.y += h;
        self.h -= h;
        out
    }

    pub fn cut_bottom(&mut self, h: f32) -> Rect {
        let h = h.min(self.h);
        self.h -= h;
        Rect::new(self.x, self.y + self.h, self.w, h)
    }

    /// A child of width `w`, centered horizontally.
    pub fn center_h(&self, w: f32) -> Rect {
        Rect::new(self.x + (self.w - w) / 2., self.y, w, self.h)
    }

    /// A child of size `w` x `h`, centered.
    pub fn center(&self, w: f32, h: f32) -> Rect {
        Rect::new(self.x + (self.w - w) / 2., self.y + (self.h - h) / 2., w, h)
    }

    pub fn with_h(&self, h: f32) -> Rect {
        Rect::new(self.x, self.y, self.w, h)
    }

    pub fn with_w(&self, w: f32) -> Rect {
        Rect::new(self.x, self.y, w, self.h)
    }

    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn expand(&self, d: f32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2. * d, self.h + 2. * d)
    }

    /// A child of height `h`, centered vertically.
    pub fn center_v(&self, h: f32) -> Rect {
        Rect::new(self.x, self.y + (self.h - h) / 2., self.w, h)
    }
}

/// sRGB color with straight alpha, components 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

pub const fn rgb(hex: u32) -> Color {
    rgba(hex, 1.0)
}

pub const fn rgba(hex: u32, a: f32) -> Color {
    Color {
        r: ((hex >> 16) & 0xff) as f32 / 255.,
        g: ((hex >> 8) & 0xff) as f32 / 255.,
        b: (hex & 0xff) as f32 / 255.,
        a,
    }
}

impl Color {
    pub const TRANSPARENT: Color = rgba(0, 0.);

    pub fn with_alpha(self, a: f32) -> Color {
        Color {
            a: self.a * a,
            ..self
        }
    }

    pub fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    pub fn to_u8(self) -> [u8; 4] {
        let c = |v: f32| (v.clamp(0., 1.) * 255. + 0.5) as u8;
        [c(self.r), c(self.g), c(self.b), c(self.a)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(10., 10., 20., 10.);
        assert!(r.contains(Point::new(10., 10.)));
        assert!(r.contains(Point::new(29.9, 19.9)));
        assert!(!r.contains(Point::new(30., 15.)));
        assert!(!r.contains(Point::new(15., 20.)));
    }

    #[test]
    fn intersect_of_disjoint_rects_is_empty() {
        let a = Rect::new(0., 0., 10., 10.);
        let b = Rect::new(20., 20., 5., 5.);
        let i = a.intersect(&b);
        assert_eq!((i.w, i.h), (0., 0.));
        assert_eq!(
            a.intersect(&Rect::new(5., 5., 10., 10.)),
            Rect::new(5., 5., 5., 5.)
        );
    }

    #[test]
    fn cuts_shrink_the_rest() {
        let mut r = Rect::new(0., 0., 100., 50.);
        assert_eq!(r.cut_left(30.), Rect::new(0., 0., 30., 50.));
        assert_eq!(r.cut_right(20.), Rect::new(80., 0., 20., 50.));
        assert_eq!(r.cut_top(10.), Rect::new(30., 0., 50., 10.));
        assert_eq!(r.cut_bottom(15.), Rect::new(30., 35., 50., 15.));
        assert_eq!(r, Rect::new(30., 10., 50., 25.));
        // Cutting more than there is takes all of it.
        assert_eq!(r.cut_left(500.).w, 50.);
        assert_eq!(r.w, 0.);
    }

    #[test]
    fn inset_never_goes_negative() {
        let r = Rect::new(0., 0., 10., 10.).inset(8., 8., 8., 8.);
        assert_eq!((r.w, r.h), (0., 0.));
        assert_eq!(
            Rect::new(0., 0., 40., 20.).center(10., 10.),
            Rect::new(15., 5., 10., 10.)
        );
    }

    #[test]
    fn colors_from_hex() {
        assert_eq!(rgb(0xff8000).to_u8(), [255, 128, 0, 255]);
        assert_eq!(rgba(0x000000, 0.5).with_alpha(0.5).a, 0.25);
    }
}
