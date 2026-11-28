//! Selection in read-only text: one block, or many lines (a log), with selection
//! across lines. Positions are (line, byte index). No layout here: widgets map
//! pointer positions to `TextPos` and paint the ranges this gives them.

use std::ops::Range;

use crate::text::word_range;

/// A position in a text of lines. Orders by line, then index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPos {
    pub line: usize,
    /// Byte index in the line.
    pub index: usize,
}

impl TextPos {
    pub const fn new(line: usize, index: usize) -> Self {
        Self { line, index }
    }
}

/// The text a selection is in.
pub trait Lines {
    fn line_count(&self) -> usize;
    /// The line's text; "" if out of range.
    fn line(&self, i: usize) -> &str;
}

impl Lines for str {
    fn line_count(&self) -> usize {
        1
    }

    fn line(&self, i: usize) -> &str {
        if i == 0 { self } else { "" }
    }
}

impl Lines for [String] {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn line(&self, i: usize) -> &str {
        self.get(i).map_or("", |s| s.as_str())
    }
}

impl Lines for Vec<String> {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn line(&self, i: usize) -> &str {
        self.get(i).map_or("", |s| s.as_str())
    }
}

/// What a drag extends by, from the click count: one click, a double or a triple click.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Unit {
    #[default]
    Char,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Where the selection started.
    pub anchor: TextPos,
    /// The end that moves.
    pub head: TextPos,
    pub unit: Unit,
    /// The far end of the word or line the drag started on (for `Word` and `Line`).
    anchor_end: TextPos,
}

impl Selection {
    pub fn caret(at: TextPos) -> Self {
        Self {
            anchor: at,
            head: at,
            unit: Unit::Char,
            anchor_end: at,
        }
    }

    /// A press at `at` with `clicks` presses in a row: a caret, the word, or the line.
    pub fn begin(text: &(impl Lines + ?Sized), at: TextPos, clicks: u8) -> Self {
        let unit = match clicks {
            0 | 1 => Unit::Char,
            2 => Unit::Word,
            _ => Unit::Line,
        };
        let r = unit_range(text, clamp(text, at), unit);
        Self {
            anchor: r.start,
            head: r.end,
            unit,
            anchor_end: r.end,
        }
    }

    /// Everything.
    pub fn all(text: &(impl Lines + ?Sized)) -> Self {
        let n = text.line_count();
        let end = TextPos::new(n.saturating_sub(1), text.line(n.saturating_sub(1)).len());
        Self {
            anchor: TextPos::default(),
            head: end,
            unit: Unit::Char,
            anchor_end: TextPos::default(),
        }
    }

    /// Move the head to `at` (a drag or a shift-click), by whole words or lines when
    /// the selection started with a double or triple click.
    pub fn extend(&mut self, text: &(impl Lines + ?Sized), at: TextPos) {
        let at = clamp(text, at);
        if self.unit == Unit::Char {
            self.head = at;
            return;
        }
        let start = self.anchor.min(self.anchor_end);
        let end = self.anchor.max(self.anchor_end);
        let r = unit_range(text, at, self.unit);
        if r.start < start {
            self.anchor = end;
            self.anchor_end = start;
            self.head = r.start;
        } else {
            self.anchor = start;
            self.anchor_end = end;
            self.head = r.end.max(end);
        }
    }

    pub fn start(&self) -> TextPos {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> TextPos {
        self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The selected bytes of one line, if any; `len` is the line's length.
    /// Lines inside a multi-line selection report `0..len` and `true` for the break.
    pub fn line_range(&self, line: usize, len: usize) -> Option<(Range<usize>, bool)> {
        let (s, e) = (self.start(), self.end());
        if self.is_empty() || line < s.line || line > e.line {
            return None;
        }
        let a = if line == s.line { s.index.min(len) } else { 0 };
        let b = if line == e.line {
            e.index.min(len)
        } else {
            len
        };
        Some((a..b, line < e.line))
    }

    /// The selected text, lines joined with `\n`.
    pub fn text(&self, text: &(impl Lines + ?Sized)) -> String {
        let (s, e) = (self.start(), self.end());
        let mut out = String::new();
        for line in s.line..=e.line.min(text.line_count().saturating_sub(1)) {
            let t = text.line(line);
            if let Some((r, _)) = self.line_range(line, t.len()) {
                out.push_str(t.get(r).unwrap_or(""));
            }
            if line < e.line {
                out.push('\n');
            }
        }
        out
    }

    /// Lines were dropped from the start of the text: shift the positions up, clamping
    /// what is gone to the first line.
    pub fn drop_lines(&mut self, n: usize) {
        for p in [&mut self.anchor, &mut self.head, &mut self.anchor_end] {
            *p = if p.line >= n {
                TextPos::new(p.line - n, p.index)
            } else {
                TextPos::default()
            };
        }
    }
}

fn clamp(text: &(impl Lines + ?Sized), at: TextPos) -> TextPos {
    let last = text.line_count().saturating_sub(1);
    if at.line > last {
        return TextPos::new(last, text.line(last).len());
    }
    TextPos::new(at.line, at.index.min(text.line(at.line).len()))
}

/// The range of a unit at a position: a point, the word, or the line (a paragraph
/// between `\n` inside a block).
fn unit_range(text: &(impl Lines + ?Sized), at: TextPos, unit: Unit) -> Range<TextPos> {
    let line = text.line(at.line);
    let i = at.index.min(line.len());
    let r = match unit {
        Unit::Char => i..i,
        Unit::Word => word_range(line, i),
        Unit::Line => {
            let start = line[..i].rfind('\n').map_or(0, |p| p + 1);
            let end = line[i..].find('\n').map_or(line.len(), |p| i + p);
            start..end
        }
    };
    TextPos::new(at.line, r.start)..TextPos::new(at.line, r.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Vec<String> {
        ["first line", "second line here", "", "fourth"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn drag_across_lines_copies_joined_text() {
        let lines = log();
        let mut s = Selection::begin(&lines, TextPos::new(0, 6), 1);
        assert!(s.is_empty());
        s.extend(&lines, TextPos::new(1, 6));
        assert_eq!(s.text(&lines), "line\nsecond");
        // Dragging back above the anchor flips the ends.
        s.extend(&lines, TextPos::new(0, 0));
        assert_eq!(s.text(&lines), "first ");
        assert_eq!(
            (s.start(), s.end()),
            (TextPos::new(0, 0), TextPos::new(0, 6))
        );
    }

    #[test]
    fn empty_lines_keep_their_breaks() {
        let lines = log();
        let mut s = Selection::begin(&lines, TextPos::new(1, 12), 1);
        s.extend(&lines, TextPos::new(3, 3));
        assert_eq!(s.text(&lines), "here\n\nfou");
        assert_eq!(s.line_range(2, 0), Some((0..0, true)));
        assert_eq!(s.line_range(3, 6), Some((0..3, false)));
        assert_eq!(s.line_range(0, 10), None);
    }

    #[test]
    fn double_click_selects_words_and_drags_by_words() {
        let lines = log();
        let mut s = Selection::begin(&lines, TextPos::new(1, 2), 2);
        assert_eq!(s.text(&lines), "second");
        s.extend(&lines, TextPos::new(1, 13));
        assert_eq!(s.text(&lines), "second line here");
        // Back before the first word: the first word stays selected.
        s.extend(&lines, TextPos::new(0, 1));
        assert_eq!(s.text(&lines), "first line\nsecond");
    }

    #[test]
    fn triple_click_selects_the_line() {
        let lines = log();
        let mut s = Selection::begin(&lines, TextPos::new(1, 3), 3);
        assert_eq!(s.text(&lines), "second line here");
        s.extend(&lines, TextPos::new(3, 0));
        assert_eq!(s.text(&lines), "second line here\n\nfourth");
        // In a block, a "line" is a paragraph.
        let block = "a b\nc d\ne";
        let s = Selection::begin(block, TextPos::new(0, 5), 3);
        assert_eq!(s.text(block), "c d");
    }

    #[test]
    fn select_all_and_single_blocks() {
        let lines = log();
        assert_eq!(
            Selection::all(&lines).text(&lines),
            "first line\nsecond line here\n\nfourth"
        );
        let block = "{\n  \"id\": 1\n}";
        assert_eq!(Selection::all(block).text(block), block);
        let empty: Vec<String> = Vec::new();
        assert_eq!(Selection::all(&empty).text(&empty), "");
    }

    #[test]
    fn dropping_lines_shifts_the_selection() {
        let lines = log();
        let mut s = Selection::begin(&lines, TextPos::new(1, 0), 1);
        s.extend(&lines, TextPos::new(3, 2));
        s.drop_lines(2);
        assert_eq!(
            (s.start(), s.end()),
            (TextPos::new(0, 0), TextPos::new(1, 2))
        );
        s.drop_lines(5);
        assert!(s.is_empty());
    }

    #[test]
    fn positions_past_the_end_are_clamped() {
        let lines = log();
        let mut s = Selection::begin(&lines, TextPos::new(3, 100), 1);
        s.extend(&lines, TextPos::new(9, 0));
        assert_eq!(s.text(&lines), "");
        let s = Selection::begin(&lines, TextPos::new(3, 100), 2);
        assert_eq!(s.text(&lines), "fourth");
    }
}
