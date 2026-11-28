//! Rect-cut layout for rows of widgets: take slices from the left or the right.

use crate::geom::Rect;

pub struct Row {
    pub rect: Rect,
    pub gap: f32,
    left: f32,
    right: f32,
}

impl Row {
    pub fn new(rect: Rect, gap: f32) -> Self {
        Self {
            rect,
            gap,
            left: rect.x,
            right: rect.right(),
        }
    }

    /// The next `w` wide slot from the left, full height.
    pub fn left(&mut self, w: f32) -> Rect {
        let r = Rect::new(self.left, self.rect.y, w, self.rect.h);
        self.left += w + self.gap;
        r
    }

    /// The next `w` wide slot from the right.
    pub fn right(&mut self, w: f32) -> Rect {
        self.right -= w;
        let r = Rect::new(self.right, self.rect.y, w, self.rect.h);
        self.right -= self.gap;
        r
    }

    /// A slot of height `h`, centered vertically, from the left.
    pub fn left_h(&mut self, w: f32, h: f32) -> Rect {
        self.left(w).center_v(h)
    }

    pub fn right_h(&mut self, w: f32, h: f32) -> Rect {
        self.right(w).center_v(h)
    }

    /// What is left between the two sides.
    pub fn rest(&self) -> Rect {
        Rect::new(
            self.left,
            self.rect.y,
            (self.right - self.left).max(0.),
            self.rect.h,
        )
    }

    pub fn skip(&mut self, w: f32) {
        self.left += w;
    }
}

/// A vertical cursor for stacked content.
pub struct Column {
    pub x: f32,
    pub y: f32,
    pub w: f32,
}

impl Column {
    pub fn new(x: f32, y: f32, w: f32) -> Self {
        Self { x, y, w }
    }

    /// Take `h` of height.
    pub fn take(&mut self, h: f32) -> Rect {
        let r = Rect::new(self.x, self.y, self.w, h);
        self.y += h;
        r
    }

    pub fn space(&mut self, h: f32) {
        self.y += h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_takes_slots_from_both_sides() {
        let mut row = Row::new(Rect::new(10., 0., 200., 40.), 8.);
        assert_eq!(row.left(50.), Rect::new(10., 0., 50., 40.));
        assert_eq!(row.right(30.), Rect::new(180., 0., 30., 40.));
        assert_eq!(row.left_h(20., 10.), Rect::new(68., 15., 20., 10.));
        assert_eq!(row.rest(), Rect::new(96., 0., 76., 40.));
        // Overlapping slots leave nothing, never a negative rest.
        row.left(500.);
        assert_eq!(row.rest().w, 0.);
    }

    #[test]
    fn column_stacks() {
        let mut c = Column::new(5., 10., 100.);
        assert_eq!(c.take(20.), Rect::new(5., 10., 100., 20.));
        c.space(4.);
        assert_eq!(c.take(10.).y, 34.);
    }
}
