//! A vertical list that lays out and paints only the rows in view.
//!
//! Rows are the same height (`new`) or each its own (`variable`). Variable rows start
//! at an estimate; the list measures the rows that come into view and keeps the
//! measured heights, so wrapped text gets its real height where it is seen.

use std::ops::Range;

use crate::geom::{Point, Rect};
use crate::input::ScrollDelta;

pub struct VirtualList {
    rows: Rows,
    /// Pixels per wheel line.
    pub line_height: f32,
    offset: f32,
    count: usize,
    viewport: Rect,
}

enum Rows {
    Fixed(f32),
    Variable {
        estimate: f32,
        heights: Vec<f32>,
        measured: Vec<bool>,
        /// Top of each row, and the total at the end; rebuilt when heights change.
        tops: Vec<f32>,
        dirty: bool,
    },
}

impl VirtualList {
    /// Rows of `row_height`; a wheel line scrolls `line_height`.
    pub fn new(row_height: f32, line_height: f32) -> Self {
        Self {
            rows: Rows::Fixed(row_height),
            line_height,
            offset: 0.,
            count: 0,
            viewport: Rect::default(),
        }
    }

    /// Rows of their own height, `estimate` until measured.
    pub fn variable(estimate: f32, line_height: f32) -> Self {
        let rows = Rows::Variable {
            estimate,
            heights: Vec::new(),
            measured: Vec::new(),
            tops: vec![0.],
            dirty: false,
        };
        Self {
            rows,
            line_height,
            offset: 0.,
            count: 0,
            viewport: Rect::default(),
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        self.viewport.contains(p)
    }

    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Apply a wheel or trackpad delta. Returns true if the offset changed.
    pub fn scroll(&mut self, delta: &ScrollDelta) -> bool {
        self.scroll_px(delta.pixels(self.line_height).1)
    }

    /// Scroll by `dy` pixels (positive: towards the top). Returns true on change.
    pub fn scroll_px(&mut self, dy: f32) -> bool {
        let old = self.offset;
        self.offset -= dy;
        self.clamp();
        self.offset != old
    }

    pub fn offset(&self) -> f32 {
        self.offset
    }

    pub fn set_offset(&mut self, offset: f32) {
        self.offset = offset;
        self.clamp();
    }

    /// Set the viewport and row count for this frame, and clamp the offset. For
    /// variable rows, use `layout_measured`.
    pub fn layout(&mut self, rect: Rect, count: usize) {
        self.viewport = rect;
        self.set_count(count);
        self.clamp();
    }

    /// `layout` for variable rows: measures the rows in view with `measure(row)`.
    /// With `stick_to_end`, the list ends up scrolled to the bottom.
    pub fn layout_measured(
        &mut self,
        rect: Rect,
        count: usize,
        stick_to_end: bool,
        mut measure: impl FnMut(usize) -> f32,
    ) {
        self.viewport = rect;
        self.set_count(count);
        // Measuring changes the heights, which can change what is in view: repeat a
        // few times (once or twice in practice).
        for _ in 0..4 {
            if stick_to_end {
                self.offset = self.max_offset();
            }
            self.clamp();
            let mut changed = false;
            let range = self.visible();
            if let Rows::Variable {
                heights,
                measured,
                dirty,
                ..
            } = &mut self.rows
            {
                for i in range {
                    if !measured[i] {
                        let h = measure(i).max(1.);
                        measured[i] = true;
                        if (heights[i] - h).abs() > 0.01 {
                            heights[i] = h;
                            changed = true;
                        }
                    }
                }
                *dirty |= changed;
            }
            if !changed {
                break;
            }
        }
        self.clamp();
    }

    /// Forget the measured heights (the width changed); they stay as estimates.
    pub fn invalidate(&mut self) {
        if let Rows::Variable { measured, .. } = &mut self.rows {
            measured.iter_mut().for_each(|m| *m = false);
        }
    }

    /// Rows were removed from the start of the data: drop their heights and keep
    /// the rest where it is on screen.
    pub fn drop_front(&mut self, n: usize) {
        let n = n.min(self.count);
        let removed = self.top(n);
        if let Rows::Variable {
            heights,
            measured,
            dirty,
            ..
        } = &mut self.rows
        {
            heights.drain(..n);
            measured.drain(..n);
            *dirty = true;
        }
        self.count -= n;
        self.offset = (self.offset - removed).max(0.);
    }

    /// Rows that intersect the viewport.
    pub fn visible(&mut self) -> Range<usize> {
        if self.count == 0 {
            return 0..0;
        }
        let first = self.row_at_offset(self.offset);
        let bottom = self.offset + self.viewport.h;
        let mut last = first;
        while last < self.count && self.top(last) < bottom {
            last += 1;
        }
        first..last.max(first)
    }

    /// The row at a window position, if it is in the viewport.
    pub fn row_at(&mut self, p: Point) -> Option<usize> {
        if p.y < self.viewport.y || p.y >= self.viewport.bottom() || self.count == 0 {
            return None;
        }
        let i = self.row_at_offset(self.offset + p.y - self.viewport.y);
        (i < self.count).then_some(i)
    }

    /// The row at a window y, clamped to the rows that exist (for drags that leave
    /// the list). None if there are no rows.
    pub fn row_near(&mut self, y: f32) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let at = self.offset + y - self.viewport.y;
        if at < 0. {
            return Some(0);
        }
        Some(self.row_at_offset(at).min(self.count - 1))
    }

    pub fn row_rect(&mut self, i: usize) -> Rect {
        let v = self.viewport;
        let h = self.row_height(i);
        Rect::new(v.x, v.y + self.top(i) - self.offset, v.w, h)
    }

    pub fn content_height(&mut self) -> f32 {
        self.top(self.count)
    }

    pub fn max_offset(&mut self) -> f32 {
        (self.content_height() - self.viewport.h).max(0.)
    }

    pub fn scroll_to_end(&mut self) {
        self.offset = self.max_offset();
    }

    pub fn at_end(&mut self) -> bool {
        self.offset >= self.max_offset() - 1.
    }

    /// Scroll as little as needed to show row `i` whole.
    pub fn scroll_into_view(&mut self, i: usize) {
        let (top, h) = (self.top(i), self.row_height(i));
        if top < self.offset {
            self.offset = top;
        } else if top + h > self.offset + self.viewport.h {
            self.offset = top + h - self.viewport.h;
        }
        self.clamp();
    }

    fn row_height(&self, i: usize) -> f32 {
        match &self.rows {
            Rows::Fixed(h) => *h,
            Rows::Variable {
                heights, estimate, ..
            } => heights.get(i).copied().unwrap_or(*estimate),
        }
    }

    /// The top of row `i` in content coordinates (`i == count`: the total height).
    fn top(&mut self, i: usize) -> f32 {
        match &mut self.rows {
            Rows::Fixed(h) => i as f32 * *h,
            Rows::Variable {
                heights,
                tops,
                dirty,
                ..
            } => {
                if *dirty || tops.len() != heights.len() + 1 {
                    tops.clear();
                    tops.push(0.);
                    let mut y = 0.;
                    for h in heights.iter() {
                        y += h;
                        tops.push(y);
                    }
                    *dirty = false;
                }
                tops[i.min(heights.len())]
            }
        }
    }

    fn row_at_offset(&mut self, y: f32) -> usize {
        match &self.rows {
            Rows::Fixed(h) => ((y / h).floor().max(0.) as usize).min(self.count),
            Rows::Variable { .. } => {
                self.top(0);
                let Rows::Variable { tops, .. } = &self.rows else {
                    unreachable!()
                };
                // The last row whose top is at or above y.
                tops.partition_point(|t| *t <= y)
                    .saturating_sub(1)
                    .min(self.count)
            }
        }
    }

    fn set_count(&mut self, count: usize) {
        self.count = count;
        if let Rows::Variable {
            estimate,
            heights,
            measured,
            dirty,
            ..
        } = &mut self.rows
            && heights.len() != count
        {
            heights.resize(count, *estimate);
            measured.resize(count, false);
            *dirty = true;
        }
    }

    fn clamp(&mut self) {
        let max = self.max_offset();
        self.offset = self.offset.clamp(0., max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(count: usize, offset: f32) -> VirtualList {
        let mut l = VirtualList::new(20., 20.);
        l.layout(Rect::new(0., 100., 300., 90.), count);
        l.set_offset(offset);
        l
    }

    #[test]
    fn fixed_rows_in_view() {
        let mut l = fixed(1000, 0.);
        // 90 px of 20 px rows: rows 0..5 (the fifth is cut).
        assert_eq!(l.visible(), 0..5);
        l.set_offset(30.);
        assert_eq!(l.visible(), 1..6);
        assert_eq!(l.row_rect(1), Rect::new(0., 90., 300., 20.));
        assert_eq!(l.row_at(Point::new(5., 100.)), Some(1));
        assert_eq!(l.row_at(Point::new(5., 99.)), None);
        assert_eq!(l.row_near(50.), Some(0));
        assert_eq!(l.row_near(100_000.), Some(999));
    }

    #[test]
    fn offsets_are_clamped() {
        let mut l = fixed(10, 0.);
        assert_eq!(l.max_offset(), 110.);
        l.set_offset(1e6);
        assert_eq!(l.offset(), 110.);
        assert!(l.at_end());
        assert_eq!(l.visible(), 5..10);
        assert!(!l.scroll_px(-50.));
        assert!(l.scroll(&ScrollDelta::Lines(0., 1.)));
        assert_eq!(l.offset(), 90.);
        let mut few = fixed(2, 50.);
        assert_eq!((few.offset(), few.visible()), (0., 0..2));
        let mut none = fixed(0, 0.);
        assert_eq!(none.visible(), 0..0);
        assert_eq!(none.row_near(0.), None);
    }

    #[test]
    fn scroll_into_view_moves_the_least() {
        let mut l = fixed(100, 0.);
        l.scroll_into_view(2);
        assert_eq!(l.offset(), 0.);
        l.scroll_into_view(10);
        assert_eq!(l.offset(), 11. * 20. - 90.);
        l.scroll_into_view(3);
        assert_eq!(l.offset(), 60.);
    }

    #[test]
    fn variable_rows_are_measured_when_seen() {
        let mut l = VirtualList::variable(20., 20.);
        let mut asked = Vec::new();
        l.layout_measured(Rect::new(0., 0., 100., 100.), 1000, false, |i| {
            asked.push(i);
            if i % 2 == 0 { 40. } else { 20. }
        });
        // At 20 px each, rows 0..5 would fill 100 px: only those are measured.
        assert_eq!(asked, [0, 1, 2, 3, 4]);
        // Measured, rows 0..3 fill it (40 + 20 + 40).
        assert_eq!(l.visible(), 0..3);
        assert_eq!(l.row_rect(2), Rect::new(0., 60., 100., 40.));
        assert_eq!(l.content_height(), 160. + 995. * 20.);
        // The next frame measures nothing new.
        l.layout_measured(Rect::new(0., 0., 100., 100.), 1000, false, |_| {
            panic!("measured again")
        });
    }

    #[test]
    fn sticking_to_the_end_measures_the_last_rows() {
        let mut l = VirtualList::variable(20., 20.);
        l.layout_measured(Rect::new(0., 0., 100., 100.), 50, true, |_| 50.);
        // The last two rows (100 px) fill the view once measured.
        assert_eq!(l.visible(), 48..50);
        assert!(l.at_end());
        assert_eq!(l.row_rect(49).bottom(), 100.);
    }

    #[test]
    fn invalidate_keeps_heights_as_estimates() {
        let mut l = VirtualList::variable(20., 20.);
        let r = Rect::new(0., 0., 100., 60.);
        // Three estimated rows fit in 60 px: all three get measured.
        l.layout_measured(r, 10, false, |_| 30.);
        assert_eq!(l.content_height(), 3. * 30. + 7. * 20.);
        l.invalidate();
        // Two rows of 30 fit now; measured at 60, one is in view.
        l.layout_measured(r, 10, false, |_| 60.);
        assert_eq!(l.visible(), 0..1);
        assert_eq!(l.content_height(), 2. * 60. + 30. + 7. * 20.);
    }

    #[test]
    fn dropping_front_rows_keeps_the_view_steady() {
        let mut l = VirtualList::variable(20., 20.);
        let r = Rect::new(0., 0., 100., 40.);
        l.layout_measured(r, 100, false, |_| 20.);
        l.set_offset(200.);
        l.layout_measured(r, 100, false, |_| 20.);
        assert_eq!(l.visible(), 10..12);
        l.drop_front(5);
        l.layout_measured(r, 95, false, |_| 20.);
        assert_eq!(l.visible(), 5..7);
        assert_eq!(l.offset(), 100.);
    }
}
