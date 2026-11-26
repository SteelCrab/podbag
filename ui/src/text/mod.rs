//! Text shaping and a per-frame layout cache (cosmic-text through glyphon), and the
//! queries widgets need on shaped text: hit testing, caret positions, selection rects.

mod edit;
mod fonts;

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use glyphon::cosmic_text::{Ellipsize, EllipsizeHeightLimit};
use glyphon::{Attrs, Buffer, Cursor, Family, FontSystem, Metrics, Shaping, Weight, Wrap, fontdb};

use crate::geom::{Point, Rect};

pub use edit::{EditBuffer, word_range};
use fonts::UiFallback;
pub use fonts::{Fonts, load_fonts};

/// Default line height as a multiple of the font size.
pub const LINE_HEIGHT: f32 = 1.618;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub weight: u16,
    /// Line box height in logical px.
    pub line_height: f32,
    pub mono: bool,
}

impl TextStyle {
    pub fn new(size: f32) -> Self {
        Self {
            size,
            weight: 400,
            line_height: size * LINE_HEIGHT,
            mono: false,
        }
    }

    pub fn weight(mut self, w: u16) -> Self {
        self.weight = w;
        self
    }

    pub fn line_height(mut self, h: f32) -> Self {
        self.line_height = h;
        self
    }

    /// Another size, keeping the line height ratio.
    pub fn with_size(mut self, size: f32) -> Self {
        self.line_height = (self.line_height / self.size * size).round();
        self.size = size;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
}

/// Shaped text, one or more lines. Cheap to clone.
///
/// Byte indices are into the text as given. Positions are relative to the top-left
/// of the first line box.
#[derive(Clone)]
pub struct TextLayout {
    pub buffer: Rc<Buffer>,
    /// Width of the widest line.
    pub width: f32,
    /// Height of all lines.
    pub height: f32,
    /// Baseline offset of the first line from the top.
    pub baseline: f32,
    /// Visual lines, after wrapping.
    pub lines: usize,
    /// Byte offset of each paragraph (text between line breaks).
    starts: Rc<[usize]>,
    len: usize,
}

impl TextLayout {
    /// Length of the text in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The closest byte index to a point.
    pub fn index_at(&self, p: Point) -> usize {
        if p.y < 0. {
            return 0;
        }
        if p.y >= self.height {
            return self.len;
        }
        match self.buffer.hit(p.x, p.y) {
            Some(c) => self.global(c),
            None => 0,
        }
    }

    /// Top-left of the caret at a byte index.
    pub fn position(&self, index: usize) -> Point {
        let c = self.cursor(index.min(self.len));
        let mut fallback = None;
        // A wrapped paragraph has several runs: prefer the one with a glyph at the index,
        // so the caret at a wrap point shows at the start of the next line.
        for run in self.buffer.layout_runs().filter(|r| r.line_i == c.line) {
            let Some(x) = run.cursor_position(&c) else {
                continue;
            };
            let p = Point::new(x, run.line_top);
            if run
                .glyphs
                .iter()
                .any(|g| c.index >= g.start && c.index < g.end)
            {
                return p;
            }
            fallback.get_or_insert(p);
        }
        fallback.unwrap_or_default()
    }

    /// X offset of a byte index on the first line.
    pub fn x_for_index(&self, index: usize) -> f32 {
        self.position(index).x
    }

    /// Closest byte index for an x offset on the first line.
    pub fn index_for_x(&self, x: f32) -> usize {
        let lh = self.buffer.metrics().line_height;
        self.index_at(Point::new(x, lh / 2.))
    }

    /// Highlight rects for a byte range, one or more per visual line. A selection
    /// that runs past the end of a paragraph also covers a little space after it,
    /// so selected empty lines show.
    pub fn selection_rects(&self, range: Range<usize>) -> Vec<Rect> {
        let mut out = Vec::new();
        if range.is_empty() {
            return out;
        }
        let (a, b) = (
            self.cursor(range.start),
            self.cursor(range.end.min(self.len)),
        );
        let nl = self.buffer.metrics().font_size * 0.35;
        let mut runs = self.buffer.layout_runs().peekable();
        while let Some(run) = runs.next() {
            if run.line_i < a.line || run.line_i > b.line {
                continue;
            }
            let mut spans: Vec<(f32, f32)> = run.highlight(a, b).collect();
            let last_of_line = runs.peek().is_none_or(|n| n.line_i != run.line_i);
            if last_of_line && run.line_i < b.line {
                let x = spans.iter().map(|(x, w)| x + w).fold(run.line_w, f32::max);
                spans.push((x, nl));
            }
            out.extend(
                spans
                    .into_iter()
                    .map(|(x, w)| Rect::new(x, run.line_top, w, run.line_height)),
            );
        }
        out
    }

    fn cursor(&self, index: usize) -> Cursor {
        let line = self
            .starts
            .partition_point(|s| *s <= index)
            .saturating_sub(1);
        Cursor::new(line, index - self.starts[line])
    }

    fn global(&self, c: Cursor) -> usize {
        let start = self.starts.get(c.line).copied().unwrap_or(self.len);
        (start + c.index).min(self.len)
    }
}

#[derive(Hash, PartialEq, Eq)]
struct Key {
    text: String,
    size: u32,
    weight: u16,
    line_height: u32,
    mono: bool,
    /// Ellipsis width, or u32::MAX.
    max_width: u32,
    /// Wrap width, or u32::MAX.
    wrap: u32,
}

pub struct TextSystem {
    pub fonts: FontSystem,
    family: String,
    mono_family: String,
    cache: HashMap<Key, (TextLayout, u64)>,
    frame: u64,
}

impl TextSystem {
    pub fn new(fonts: Fonts) -> Self {
        Self {
            fonts: fonts.system,
            family: fonts.family,
            mono_family: fonts.mono_family,
            cache: HashMap::new(),
            frame: 0,
        }
    }

    /// Switch the UI language: Han characters then use that language's font
    /// (Japanese vs. Chinese shapes). Clears the layout cache.
    pub fn set_language(&mut self, lang: &str) {
        if self.fonts.locale() == lang {
            return;
        }
        let empty = FontSystem::new_with_locale_and_db("en".into(), fontdb::Database::new());
        let db = std::mem::replace(&mut self.fonts, empty)
            .into_locale_and_db()
            .1;
        self.fonts = FontSystem::new_with_locale_and_db_and_fallback(lang.into(), db, UiFallback);
        self.cache.clear();
    }

    /// Shape one line. With `max_width`, it ends in an ellipsis when too long.
    pub fn layout(&mut self, text: &str, style: TextStyle, max_width: Option<f32>) -> TextLayout {
        self.shape(text, style, max_width, None)
    }

    /// Shape text wrapped at `width`: breaks between words, and inside a word only
    /// when it does not fit on a line by itself.
    pub fn layout_wrapped(&mut self, text: &str, style: TextStyle, width: f32) -> TextLayout {
        self.shape(text, style, None, Some(width))
    }

    fn shape(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
        wrap: Option<f32>,
    ) -> TextLayout {
        let px = |w: Option<f32>| w.map_or(u32::MAX, |w| w.max(0.).round() as u32);
        let key = Key {
            text: text.to_owned(),
            size: style.size.to_bits(),
            weight: style.weight,
            line_height: style.line_height.to_bits(),
            mono: style.mono,
            max_width: px(max_width),
            wrap: px(wrap),
        };
        let frame = self.frame;
        if let Some((layout, used)) = self.cache.get_mut(&key) {
            *used = frame;
            return layout.clone();
        }
        let metrics = Metrics::new(style.size, style.line_height);
        let buffer = if self.fonts.db().is_empty() {
            // Without any font cosmic-text cannot shape: the text takes no room.
            Buffer::new_empty(metrics)
        } else {
            self.shape_buffer(text, style, metrics, max_width, wrap)
        };
        let mut starts = Vec::with_capacity(buffer.lines.len().max(1));
        let mut at = 0;
        for line in &buffer.lines {
            starts.push(at);
            at += line.text().len() + line.ending().as_str().len();
        }
        if starts.is_empty() {
            starts.push(0);
        }
        let mut width: f32 = 0.;
        let mut lines = 0;
        let mut baseline = style.line_height * 0.75;
        for run in buffer.layout_runs() {
            if lines == 0 {
                baseline = run.line_y;
            }
            width = width.max(run.line_w);
            lines += 1;
        }
        let height = style.line_height * lines.max(1) as f32;
        let layout = TextLayout {
            buffer: Rc::new(buffer),
            width,
            height,
            baseline,
            lines: lines.max(1),
            starts: starts.into(),
            len: text.len(),
        };
        self.cache.insert(key, (layout.clone(), frame));
        layout
    }

    fn shape_buffer(
        &mut self,
        text: &str,
        style: TextStyle,
        metrics: Metrics,
        max_width: Option<f32>,
        wrap: Option<f32>,
    ) -> Buffer {
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        if let Some(w) = wrap {
            buffer.set_wrap(Wrap::WordOrGlyph);
            buffer.set_size(Some(w.max(1.)), None);
        } else {
            buffer.set_wrap(Wrap::None);
            if let Some(w) = max_width {
                buffer.set_size(Some(w.max(0.)), None);
                buffer.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
            }
        }
        let family = if style.mono {
            &self.mono_family
        } else {
            &self.family
        };
        let attrs = Attrs::new()
            .family(Family::Name(family))
            .weight(Weight(style.weight));
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    /// Drop layouts that were not used in the last two frames, once the cache is big.
    pub fn end_frame(&mut self) {
        if self.cache.len() > 4096 {
            let keep = self.frame.saturating_sub(1);
            self.cache.retain(|_, (_, used)| *used >= keep);
        }
        self.frame += 1;
    }
}

/// The lines of a text block: split at `\n`, `\r\n` or `\r`, without the breaks.
pub fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    std::iter::from_fn(move || {
        let s = rest?;
        match s.find(['\n', '\r']) {
            Some(i) => {
                let skip = if s[i..].starts_with("\r\n") { 2 } else { 1 };
                rest = Some(&s[i + skip..]);
                Some(&s[..i])
            }
            None => {
                rest = None;
                Some(s)
            }
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Run `f` with the system fonts (loaded once per test thread). Skipped on a
    /// machine without fonts.
    pub fn with_fonts(f: impl FnOnce(&mut TextSystem)) {
        thread_local! {
            static TEXT: RefCell<Option<TextSystem>> = RefCell::new({
                let fonts = load_fonts();
                (fonts.faces > 0).then(|| TextSystem::new(fonts))
            });
        }
        TEXT.with_borrow_mut(|t| match t {
            Some(ts) => f(ts),
            None => eprintln!("no system fonts: skipping a shaping test"),
        });
    }

    #[test]
    fn split_lines_handles_every_break() {
        let v: Vec<&str> = split_lines("a\nb\r\nc\rd").collect();
        assert_eq!(v, ["a", "b", "c", "d"]);
        let v: Vec<&str> = split_lines("x\n").collect();
        assert_eq!(v, ["x", ""]);
        assert_eq!(split_lines("").collect::<Vec<_>>(), [""]);
    }

    #[test]
    fn wrapping_adds_lines_and_keeps_within_the_width() {
        with_fonts(|ts| {
            let style = TextStyle::new(14.).line_height(20.);
            let text = "The quick brown fox jumps over the lazy dog, again and again and again.";
            let one = ts.layout(text, style, None);
            assert_eq!(one.lines, 1);
            assert_eq!(one.height, 20.);
            let wrapped = ts.layout_wrapped(text, style, one.width / 3.);
            assert!(wrapped.lines >= 3, "{} lines", wrapped.lines);
            assert_eq!(wrapped.height, 20. * wrapped.lines as f32);
            assert!(wrapped.width <= one.width / 3. + 0.5);
            // Wide enough: one line again.
            assert_eq!(ts.layout_wrapped(text, style, one.width + 10.).lines, 1);
        });
    }

    #[test]
    fn wrapping_breaks_between_words_first() {
        with_fonts(|ts| {
            let style = TextStyle::new(14.).line_height(20.);
            let word = ts.layout("alpha", style, None).width;
            let l = ts.layout_wrapped("alpha beta", style, word + 4.);
            assert_eq!(l.lines, 2);
            // The second line starts at "beta".
            let p = l.position(6);
            assert_eq!(p.y, 20.);
            assert!(p.x < 1.);
        });
    }

    #[test]
    fn long_words_and_cjk_break_anywhere() {
        with_fonts(|ts| {
            let style = TextStyle::new(14.).line_height(20.);
            let long = "a".repeat(200);
            assert!(ts.layout_wrapped(&long, style, 100.).lines > 3);
            // No spaces in Korean-less CJK text: still wraps.
            let cjk = "日本語の文章は単語の間に空白がありませんが折り返す必要があります";
            let w = ts.layout(cjk, style, None).width;
            assert!(ts.layout_wrapped(cjk, style, w / 4.).lines >= 4);
        });
    }

    #[test]
    fn hard_breaks_start_new_lines() {
        with_fonts(|ts| {
            let style = TextStyle::new(14.).line_height(20.);
            let l = ts.layout_wrapped("one\ntwo\r\n\nfour", style, 500.);
            assert_eq!(l.lines, 4);
            assert_eq!(l.height, 80.);
            // Byte 4 is "two", on the second line; byte 9 is the empty third line.
            assert_eq!(l.position(4).y, 20.);
            assert_eq!(l.position(9).y, 40.);
            assert_eq!(l.index_at(Point::new(0., 65.)), 10);
            assert_eq!(l.index_at(Point::new(1000., 25.)), 7);
        });
    }

    #[test]
    fn hit_testing_round_trips_positions() {
        with_fonts(|ts| {
            let style = TextStyle::new(14.).line_height(20.).mono();
            let text = "hello world";
            let l = ts.layout(text, style, None);
            for i in [0, 1, 5, 6, 11] {
                let p = l.position(i);
                assert_eq!(l.index_at(Point::new(p.x + 0.5, 10.)), i, "index {i}");
            }
            assert_eq!(l.index_at(Point::new(-5., 10.)), 0);
            assert_eq!(l.index_at(Point::new(10_000., 10.)), text.len());
            assert_eq!(l.index_at(Point::new(5., -3.)), 0);
            assert_eq!(l.index_at(Point::new(5., 50.)), text.len());
        });
    }

    #[test]
    fn selection_rects_cover_each_visual_line() {
        with_fonts(|ts| {
            let style = TextStyle::new(14.).line_height(20.);
            let l = ts.layout_wrapped("first line\nsecond", style, 500.);
            let rects = l.selection_rects(2..14);
            // Part of line one, its break, and part of line two.
            assert_eq!(rects.iter().filter(|r| r.y == 0.).count(), 2);
            assert_eq!(rects.iter().filter(|r| r.y == 20.).count(), 1);
            assert!(l.selection_rects(3..3).is_empty());
        });
    }
}
