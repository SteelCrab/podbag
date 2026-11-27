//! Image compression in podbag's store (`podbag system compression --json`): whether
//! images that no container used for a while are compressed, and after how long.

use serde_json::Value;

/// The engine's setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Compress images that no container used for a while.
    Idle,
    Off,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Idle => "idle",
            Mode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "idle" => Some(Mode::Idle),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }
}

/// What the engine reports. A reply without a known mode does not say what happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Compression {
    pub mode: Option<Mode>,
    /// Seconds an image stays unused before it is compressed (0: at once).
    pub after: Option<u64>,
}

impl Compression {
    /// From `{"mode": "idle", "after": 604800}`; anything else gives no mode.
    pub fn from_json(v: &Value) -> Compression {
        Compression {
            mode: v["mode"].as_str().and_then(Mode::parse),
            after: v["after"].as_u64(),
        }
    }

    pub fn is_on(&self) -> bool {
        self.mode == Some(Mode::Idle)
    }
}

/// An image's compressed size on disk, its plain size, and the space saved in whole
/// percent. None when the engine gave no plain size.
pub fn saved(sizes: &podbag::client::CompressionSizes) -> Option<(u64, u64, u64)> {
    if sizes.original_size == 0 {
        return None;
    }
    let ratio = sizes.size as f64 / sizes.original_size as f64;
    let percent = ((1. - ratio) * 100.).round().max(0.) as u64;
    Some((sizes.size, sizes.original_size, percent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use podbag::client::CompressionSizes;
    use serde_json::json;

    #[test]
    fn the_reply_gives_the_mode_and_the_age() {
        let c = Compression::from_json(&json!({"mode": "idle", "after": 129600}));
        assert_eq!((c.mode, c.after), (Some(Mode::Idle), Some(129_600)));
        assert!(c.is_on());
        let off = Compression::from_json(&json!({"mode": "off", "after": 0}));
        assert_eq!((off.mode, off.after), (Some(Mode::Off), Some(0)));
        assert!(!off.is_on());
        for broken in [json!(null), json!({"mode": "fast"}), json!({"after": 5})] {
            assert_eq!(Compression::from_json(&broken).mode, None, "{broken}");
        }
    }

    #[test]
    fn the_space_saved_is_in_whole_percent() {
        let sizes = |size, original_size| CompressionSizes {
            size,
            original_size,
        };
        assert_eq!(
            saved(&sizes(3_900_000, 8_520_000)),
            Some((3_900_000, 8_520_000, 54))
        );
        // A bigger file than before saves nothing; no plain size is no compression.
        assert_eq!(saved(&sizes(110, 100)).map(|s| s.2), Some(0));
        assert_eq!(saved(&sizes(10, 0)), None);
    }
}
