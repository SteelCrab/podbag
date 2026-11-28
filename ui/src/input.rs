//! Input types that do not depend on a window: key presses, scroll deltas and the
//! clipboard. Real input is converted from winit; tests and `Synthetic` build them
//! directly.

use winit::event::KeyEvent;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollDelta {
    /// Lines (x, y), from a classic mouse wheel.
    Lines(f32, f32),
    /// Logical pixels (x, y), from a trackpad.
    Pixels(f32, f32),
}

impl ScrollDelta {
    /// Pixels on both axes; wheel lines are `line_px` each.
    pub fn pixels(self, line_px: f32) -> (f32, f32) {
        match self {
            ScrollDelta::Lines(x, y) => (x * line_px, y * line_px),
            ScrollDelta::Pixels(x, y) => (x, y),
        }
    }
}

/// A key press, with the modifiers held at the time.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyPress {
    /// The key as the keyboard layout reports it.
    pub logical_key: Key,
    /// The text the key types, if any.
    pub text: Option<String>,
    /// The Latin letter for shortcuts (see `shortcut_letter`).
    pub letter: Option<char>,
    pub mods: ModifiersState,
    pub repeat: bool,
}

impl KeyPress {
    /// A named key (Tab, Enter, arrows...) without modifiers.
    pub fn named(key: NamedKey) -> Self {
        let text = match key {
            NamedKey::Space => Some(" ".to_string()),
            NamedKey::Enter => Some("\r".to_string()),
            NamedKey::Tab => Some("\t".to_string()),
            _ => None,
        };
        let logical_key = if key == NamedKey::Space {
            Key::Character(" ".into())
        } else {
            Key::Named(key)
        };
        Self {
            logical_key,
            text,
            letter: None,
            mods: ModifiersState::empty(),
            repeat: false,
        }
    }

    /// A key that types `s`, without modifiers.
    pub fn char(s: &str) -> Self {
        let letter = s
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic())
            .map(|c| c.to_ascii_lowercase());
        Self {
            logical_key: Key::Character(s.into()),
            text: Some(s.into()),
            letter,
            mods: ModifiersState::empty(),
            repeat: false,
        }
    }

    /// The platform shortcut for a letter: Cmd+`letter` on macOS, Ctrl+`letter` elsewhere.
    pub fn shortcut(letter: char) -> Self {
        let mods = if cfg!(target_os = "macos") {
            ModifiersState::SUPER
        } else {
            ModifiersState::CONTROL
        };
        Self::char(&letter.to_string()).with_mods(mods)
    }

    pub fn with_mods(mut self, mods: ModifiersState) -> Self {
        self.mods = mods;
        self
    }

    pub fn from_winit(event: &KeyEvent, mods: ModifiersState) -> Self {
        Self {
            logical_key: event.logical_key.clone(),
            text: event.text.as_ref().map(|t| t.to_string()),
            letter: letter_of(&event.logical_key, event.physical_key),
            mods,
            repeat: event.repeat,
        }
    }

    /// True for this named key. Space counts as `NamedKey::Space` on every layout.
    pub fn is(&self, key: NamedKey) -> bool {
        match &self.logical_key {
            Key::Named(k) => *k == key,
            Key::Character(s) => key == NamedKey::Space && s == " ",
            _ => false,
        }
    }

    /// No Ctrl, Alt or Cmd held (Shift is allowed).
    pub fn plain(&self) -> bool {
        !self.mods.control_key() && !self.mods.alt_key() && !self.mods.super_key()
    }
}

/// The letter of a shortcut such as Cmd+A. Non-Latin layouts (e.g. Korean) report
/// their own letter as the logical key, so this falls back to the physical key.
pub fn shortcut_letter(key: &KeyPress) -> Option<char> {
    key.letter
}

/// The platform's shortcut modifier: Cmd on macOS, Ctrl elsewhere.
pub fn shortcut_modifier(mods: ModifiersState) -> bool {
    if cfg!(target_os = "macos") {
        mods.super_key()
    } else {
        mods.control_key()
    }
}

fn letter_of(logical: &Key, physical: PhysicalKey) -> Option<char> {
    if let Key::Character(s) = logical
        && let Some(c) = s.chars().next().filter(|c| c.is_ascii_alphabetic())
    {
        return Some(c.to_ascii_lowercase());
    }
    use KeyCode::*;
    const KEYS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    KEYS.iter()
        .position(|k| *k == code)
        .map(|i| (b'a' + i as u8) as char)
}

/// The system clipboard, opened on first use, or an in-memory one for tests.
pub struct Clipboard(Inner);

enum Inner {
    System(Option<arboard::Clipboard>),
    Memory(Option<String>),
}

impl Default for Clipboard {
    fn default() -> Self {
        Self(Inner::System(None))
    }
}

impl Clipboard {
    /// A clipboard that never touches the system's.
    pub fn in_memory() -> Self {
        Self(Inner::Memory(None))
    }

    pub fn read(&mut self) -> Option<String> {
        match &mut self.0 {
            Inner::System(c) => open(c).and_then(|c| c.get_text().ok()),
            Inner::Memory(m) => m.clone(),
        }
    }

    pub fn write(&mut self, text: String) {
        match &mut self.0 {
            Inner::System(c) => {
                if let Some(c) = open(c) {
                    let _ = c.set_text(text);
                }
            }
            Inner::Memory(m) => *m = Some(text),
        }
    }
}

// Kept open for the app's life: on X11 the clipboard content lives in this process.
fn open(c: &mut Option<arboard::Clipboard>) -> Option<&mut arboard::Clipboard> {
    if c.is_none() {
        *c = arboard::Clipboard::new().ok();
    }
    c.as_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_fall_back_to_the_physical_key() {
        let hangul = Key::Character("ㅁ".into());
        assert_eq!(
            letter_of(&hangul, PhysicalKey::Code(KeyCode::KeyA)),
            Some('a')
        );
        assert_eq!(
            letter_of(
                &Key::Character("C".into()),
                PhysicalKey::Code(KeyCode::KeyJ)
            ),
            Some('c')
        );
        assert_eq!(
            letter_of(
                &Key::Named(NamedKey::Enter),
                PhysicalKey::Code(KeyCode::Enter)
            ),
            None
        );
    }

    #[test]
    fn space_is_a_named_key_too() {
        assert!(KeyPress::named(NamedKey::Space).is(NamedKey::Space));
        assert!(KeyPress::char(" ").is(NamedKey::Space));
        assert!(!KeyPress::char("a").is(NamedKey::Space));
        assert!(KeyPress::named(NamedKey::Tab).plain());
        assert!(!KeyPress::shortcut('c').plain());
        assert!(shortcut_modifier(KeyPress::shortcut('c').mods));
    }

    #[test]
    fn memory_clipboard_round_trips() {
        let mut c = Clipboard::in_memory();
        assert_eq!(c.read(), None);
        c.write("hello".into());
        assert_eq!(c.read().as_deref(), Some("hello"));
    }

    /// Uses the real clipboard (and puts its text back): run it by hand with
    /// `cargo test -p podbag-ui -- --ignored` on a desktop session.
    #[test]
    #[ignore]
    fn system_clipboard_round_trips() {
        let mut c = Clipboard::default();
        let saved = c.read();
        c.write("podbag-ui 클립보드 テスト".into());
        assert_eq!(c.read().as_deref(), Some("podbag-ui 클립보드 テスト"));
        if let Some(s) = saved {
            c.write(s);
        }
    }

    #[test]
    fn wheel_lines_become_pixels() {
        assert_eq!(ScrollDelta::Lines(0., -2.).pixels(20.), (0., -40.));
        assert_eq!(ScrollDelta::Pixels(3., 4.).pixels(20.), (3., 4.));
    }
}
