//! The editing model of a text field, without layout or painting:
//! content, selection, IME composition, keys and the clipboard.
//! The editing logic follows GPUI's `examples/input.rs` (Apache-2.0).

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use winit::event::Ime;
use winit::keyboard::NamedKey;

use crate::input::{Clipboard, KeyPress, shortcut_modifier};

#[derive(Clone, Debug, Default)]
pub struct EditBuffer {
    pub content: String,
    pub multiline: bool,
    /// The selection; empty for a plain caret.
    pub selected: Range<usize>,
    /// The caret is at the start of the selection.
    pub reversed: bool,
    /// IME composition (preedit) range in `content`.
    pub marked: Option<Range<usize>>,
}

impl EditBuffer {
    pub fn new(text: &str) -> Self {
        let mut b = Self::default();
        b.set_text(text);
        b
    }

    /// Replace the whole content; the caret goes to the end.
    pub fn set_text(&mut self, text: &str) {
        self.content = text.into();
        self.selected = text.len()..text.len();
        self.reversed = false;
        self.marked = None;
    }

    pub fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    pub fn selected_text(&self) -> &str {
        &self.content[self.selected.clone()]
    }

    pub fn select_all(&mut self) {
        self.move_to(0);
        self.select_to(self.content.len());
    }

    pub fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        self.selected = offset..offset;
        self.reversed = false;
    }

    /// Move the caret end of the selection, keeping the other end.
    pub fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
    }

    /// Select the word at `offset` (a double click).
    pub fn select_word(&mut self, offset: usize) {
        let r = word_range(&self.content, offset);
        self.move_to(r.start);
        self.select_to(r.end);
    }

    /// Replace the composition, or else the selection, with `text`; drop the composition.
    pub fn replace(&mut self, text: &str) {
        let range = self.marked.clone().unwrap_or(self.selected.clone());
        self.content.replace_range(range.clone(), text);
        let end = range.start + text.len();
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
    }

    /// Handle a key press. None: not for this field. Some(changed): handled, and
    /// whether the content changed.
    pub fn key(&mut self, key: &KeyPress, clip: &mut Clipboard) -> Option<bool> {
        if self.marked.is_some() {
            return Some(false); // the IME owns the keys while composing
        }
        let old = self.content.clone();
        let shift = key.mods.shift_key();
        // Word moves: Alt on macOS, Ctrl elsewhere. Line moves: Cmd on macOS.
        let mac = cfg!(target_os = "macos");
        let by_word = if mac {
            key.mods.alt_key()
        } else {
            key.mods.control_key()
        };
        let to_edge = mac && key.mods.super_key();
        let line = |b: &Self| {
            let at = b.cursor();
            let start = b.content[..at].rfind('\n').map_or(0, |i| i + 1);
            let end = b.content[at..]
                .find('\n')
                .map_or(b.content.len(), |i| at + i);
            start..end
        };
        let left = |b: &Self| {
            if to_edge && b.multiline {
                line(b).start
            } else if to_edge {
                0
            } else if by_word {
                previous_word(&b.content, b.cursor())
            } else {
                previous_boundary(&b.content, b.cursor())
            }
        };
        let right = |b: &Self| {
            if to_edge && b.multiline {
                line(b).end
            } else if to_edge {
                b.content.len()
            } else if by_word {
                next_word(&b.content, b.cursor())
            } else {
                next_boundary(&b.content, b.cursor())
            }
        };
        if key.is(NamedKey::Backspace) {
            if self.selected.is_empty() {
                self.select_to(left(self));
            }
            self.replace("");
        } else if key.is(NamedKey::Delete) {
            if self.selected.is_empty() {
                self.select_to(right(self));
            }
            self.replace("");
        } else if key.is(NamedKey::ArrowLeft) {
            if shift {
                self.select_to(left(self));
            } else if self.selected.is_empty() || by_word || to_edge {
                self.move_to(left(self));
            } else {
                self.move_to(self.selected.start);
            }
        } else if key.is(NamedKey::ArrowRight) {
            if shift {
                self.select_to(right(self));
            } else if self.selected.is_empty() || by_word || to_edge {
                self.move_to(right(self));
            } else {
                self.move_to(self.selected.end);
            }
        } else if key.is(NamedKey::Home) || key.is(NamedKey::End) {
            let bounds = if self.multiline && key.plain() {
                line(self)
            } else {
                0..self.content.len()
            };
            let at = if key.is(NamedKey::Home) {
                bounds.start
            } else {
                bounds.end
            };
            if shift {
                self.select_to(at)
            } else {
                self.move_to(at)
            }
        } else if self.multiline && key.plain() && key.is(NamedKey::Enter) {
            self.replace("\n");
        } else if shortcut_modifier(key.mods) {
            match key.letter {
                Some('a') => self.select_all(),
                Some('c') => {
                    if !self.selected.is_empty() {
                        clip.write(self.selected_text().to_string());
                    }
                }
                Some('x') => {
                    if !self.selected.is_empty() {
                        clip.write(self.selected_text().to_string());
                        self.replace("");
                    }
                }
                Some('v') => {
                    if let Some(text) = clip.read() {
                        if self.multiline {
                            self.replace(&text.replace("\r\n", "\n").replace('\r', "\n"));
                        } else {
                            self.replace(&single_line(&text));
                        }
                    }
                }
                _ => return None,
            }
        } else {
            match &key.text {
                Some(t)
                    if !key.mods.control_key()
                        && !key.mods.super_key()
                        && !t.chars().any(char::is_control) =>
                {
                    self.replace(t);
                }
                _ => return None,
            }
        }
        Some(old != self.content)
    }

    /// Handle IME composition. Returns true if the content or the composition changed.
    pub fn ime(&mut self, ime: &Ime) -> bool {
        match ime {
            Ime::Preedit(text, cursor) => {
                if text.is_empty() && self.marked.is_none() {
                    return false;
                }
                let range = self.marked.clone().unwrap_or(self.selected.clone());
                self.content.replace_range(range.clone(), text);
                if text.is_empty() {
                    self.marked = None;
                    self.selected = range.start..range.start;
                } else {
                    self.marked = Some(range.start..range.start + text.len());
                    self.selected = match cursor {
                        Some((a, b)) => range.start + a..range.start + b,
                        None => range.start + text.len()..range.start + text.len(),
                    };
                }
            }
            Ime::Commit(text) => self.replace(text),
            Ime::Disabled => {
                // The composition is dropped, like the IME does.
                if let Some(m) = self.marked.take() {
                    self.content.replace_range(m.clone(), "");
                    self.selected = m.start..m.start;
                }
            }
            Ime::Enabled => return false,
        }
        self.reversed = false;
        true
    }
}

/// Pasted text in a single-line field: line breaks become spaces.
fn single_line(text: &str) -> String {
    let t = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    t.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

/// The grapheme boundary before `offset`.
pub fn previous_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(i, _)| (i < offset).then_some(i))
        .unwrap_or(0)
}

/// The grapheme boundary after `offset`.
pub fn next_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(i, _)| (i > offset).then_some(i))
        .unwrap_or(text.len())
}

/// The word (or run of spaces, or punctuation) at a byte offset, as a double click
/// selects it. At the end of the text, the last word.
pub fn word_range(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let mut last = 0..0;
    for (i, w) in text.split_word_bound_indices() {
        let r = i..i + w.len();
        if offset < r.end {
            // On the space right after a word, take the word (clicks land there often).
            if is_space(w) && offset == r.start && !last.is_empty() {
                return last;
            }
            return r;
        }
        last = r;
    }
    last
}

/// The start of the word before `offset` (Alt+Left on macOS, Ctrl+Left elsewhere).
pub fn previous_word(text: &str, offset: usize) -> usize {
    text.split_word_bound_indices()
        .rev()
        .find(|(i, w)| *i < offset && is_word(w))
        .map_or(0, |(i, _)| i)
}

/// The end of the word after `offset`.
pub fn next_word(text: &str, offset: usize) -> usize {
    text.split_word_bound_indices()
        .find(|(i, w)| i + w.len() > offset && is_word(w))
        .map_or(text.len(), |(i, w)| i + w.len())
}

fn is_space(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

fn is_word(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::ModifiersState;

    fn press(b: &mut EditBuffer, key: KeyPress) -> Option<bool> {
        b.key(&key, &mut Clipboard::in_memory())
    }

    #[test]
    fn typing_and_backspace_work_on_graphemes() {
        let mut b = EditBuffer::new("");
        for c in ["h", "é", "👍🏽"] {
            assert_eq!(press(&mut b, KeyPress::char(c)), Some(true));
        }
        assert_eq!(b.content, "hé👍🏽");
        press(&mut b, KeyPress::named(NamedKey::Backspace));
        assert_eq!(b.content, "hé");
        press(&mut b, KeyPress::named(NamedKey::ArrowLeft));
        press(&mut b, KeyPress::named(NamedKey::Delete));
        assert_eq!(b.content, "h");
        assert_eq!(b.cursor(), 1);
    }

    #[test]
    fn shift_arrows_extend_both_ways() {
        let mut b = EditBuffer::new("abcd");
        b.move_to(2);
        let shift = ModifiersState::SHIFT;
        press(
            &mut b,
            KeyPress::named(NamedKey::ArrowLeft).with_mods(shift),
        );
        press(
            &mut b,
            KeyPress::named(NamedKey::ArrowLeft).with_mods(shift),
        );
        assert_eq!((b.selected.clone(), b.reversed), (0..2, true));
        for _ in 0..3 {
            press(
                &mut b,
                KeyPress::named(NamedKey::ArrowRight).with_mods(shift),
            );
        }
        assert_eq!((b.selected.clone(), b.reversed), (2..3, false));
        // A plain arrow collapses to the selection's edge.
        press(&mut b, KeyPress::named(NamedKey::ArrowLeft));
        assert_eq!(b.selected, 2..2);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut b = EditBuffer::new("hello world");
        b.select_word(1);
        assert_eq!(b.selected_text(), "hello");
        press(&mut b, KeyPress::char("J"));
        assert_eq!(b.content, "J world");
    }

    #[test]
    fn clipboard_shortcuts() {
        let mut clip = Clipboard::in_memory();
        let mut b = EditBuffer::new("one\ntwo");
        b.key(&KeyPress::shortcut('a'), &mut clip);
        b.key(&KeyPress::shortcut('x'), &mut clip);
        assert_eq!(b.content, "");
        assert_eq!(clip.read().as_deref(), Some("one\ntwo"));
        b.key(&KeyPress::shortcut('v'), &mut clip);
        assert_eq!(b.content, "one two");
        assert_eq!(b.key(&KeyPress::shortcut('q'), &mut clip), None);
    }

    #[test]
    fn ime_composition_replaces_in_place() {
        // Korean: ㅎ → 하 → 한, then commit.
        let mut b = EditBuffer::new("a");
        for p in ["ㅎ", "하", "한"] {
            assert!(b.ime(&Ime::Preedit(p.into(), Some((p.len(), p.len())))));
            assert_eq!(b.marked, Some(1..1 + p.len()));
        }
        assert_eq!(b.content, "a한");
        // Keys go to the IME while composing.
        assert_eq!(
            press(&mut b, KeyPress::named(NamedKey::Backspace)),
            Some(false)
        );
        b.ime(&Ime::Preedit(String::new(), None));
        b.ime(&Ime::Commit("한".into()));
        assert_eq!(
            (b.content.as_str(), b.marked.clone(), b.cursor()),
            ("a한", None, 4)
        );
    }

    #[test]
    fn ime_japanese_conversion_keeps_its_cursor_range() {
        let mut b = EditBuffer::new("");
        b.ime(&Ime::Preedit("にほんご".into(), Some((0, 12))));
        assert_eq!(b.selected, 0..12);
        b.ime(&Ime::Preedit("日本語".into(), Some((9, 9))));
        assert_eq!(b.content, "日本語");
        b.ime(&Ime::Commit("日本語".into()));
        assert_eq!((b.content.as_str(), b.cursor()), ("日本語", 9));
        // Chinese pinyin dropped by the IME (Escape): nothing stays.
        b.ime(&Ime::Preedit("ni".into(), None));
        b.ime(&Ime::Disabled);
        assert_eq!(b.content, "日本語");
    }

    #[test]
    fn word_ranges() {
        let t = "docker run -it alpine:3.20";
        assert_eq!(&t[word_range(t, 2)], "docker");
        assert_eq!(&t[word_range(t, 6)], "docker");
        assert_eq!(&t[word_range(t, 12)], "it");
        // Numbers with dots are one word (UAX #29).
        assert_eq!(&t[word_range(t, t.len())], "3.20");
        assert_eq!(word_range("", 0), 0..0);
        // CJK runs: one word per ideograph run (UAX #29 keeps Hangul syllables together).
        let k = "컨테이너 실행";
        assert_eq!(&k[word_range(k, 0)], "컨테이너");
    }

    #[test]
    fn word_moves_skip_punctuation_and_spaces() {
        let t = "foo, bar baz";
        assert_eq!(next_word(t, 0), 3);
        assert_eq!(next_word(t, 3), 8);
        assert_eq!(previous_word(t, 12), 9);
        assert_eq!(previous_word(t, 9), 5);
        assert_eq!(previous_word(t, 2), 0);
    }

    #[test]
    fn pasted_lines_join() {
        assert_eq!(single_line("a\r\nb\nc\n"), "a b c");
    }
}
