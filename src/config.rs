//! User settings from `<config dir>/config.toml`.

use crate::cgroup::{Budget, parse_budget};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};

/// The default image store limit.
pub const DEFAULT_STORE_LIMIT: u64 = 10 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Bytes of image content to keep before the least recently used images are removed.
    pub store_limit: u64,
    /// How much memory all containers may use together.
    pub memory_budget: Budget,
    /// The default network's subnet.
    pub default_subnet: String,
    /// How root mode mounts images.
    pub image_mount: ImageMount,
    /// One keeper process watches the detached containers; without it each
    /// container has a shim process of its own.
    pub keeper: bool,
    /// How long the keeper stays after its last container, in milliseconds.
    pub keeper_linger_ms: u64,
    /// Whether unused images are compressed in the store.
    pub image_compression: ImageCompression,
    /// How long an image must stay unused before it is compressed, in seconds.
    pub compress_idle_after: u64,
}

/// How long an image must stay unused before `image_compression = "idle"` freezes it: 7 days.
pub const DEFAULT_COMPRESS_IDLE_AFTER: u64 = 7 * 24 * 3600;

/// Whether the keeper is used when nothing says otherwise.
pub const KEEPER_DEFAULT: bool = true;

/// `image_mount`: `auto` uses composefs when podbag runs as root and the kernel passes
/// the probe, hard-link trees otherwise; `hardlink` always uses hard-link trees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageMount {
    #[default]
    Auto,
    Hardlink,
}

impl ImageMount {
    fn parse(value: &str) -> std::result::Result<ImageMount, String> {
        match value {
            "auto" => Ok(ImageMount::Auto),
            "hardlink" => Ok(ImageMount::Hardlink),
            other => Err(format!(
                "image_mount is \"{other}\"; it must be \"auto\" or \"hardlink\""
            )),
        }
    }
}

/// `image_compression`: `off` never compresses; `idle` compresses images nobody has
/// used for `compress_idle_after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCompression {
    Off,
    Idle,
}

impl ImageCompression {
    fn parse(value: &str) -> std::result::Result<ImageCompression, String> {
        match value {
            "off" => Ok(ImageCompression::Off),
            "idle" => Ok(ImageCompression::Idle),
            other => Err(format!(
                "image_compression is \"{other}\"; it must be \"off\" or \"idle\""
            )),
        }
    }

    /// The setting's value: `off` or `idle`.
    pub fn name(self) -> &'static str {
        match self {
            ImageCompression::Off => "off",
            ImageCompression::Idle => "idle",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            store_limit: DEFAULT_STORE_LIMIT,
            memory_budget: Budget::default(),
            default_subnet: "10.77.0.0/16".to_string(),
            image_mount: ImageMount::Auto,
            keeper: KEEPER_DEFAULT,
            keeper_linger_ms: 10_000,
            image_compression: ImageCompression::Off,
            compress_idle_after: DEFAULT_COMPRESS_IDLE_AFTER,
        }
    }
}

fn parse_switch(value: &str) -> std::result::Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(format!("\"{other}\" is neither true nor false")),
    }
}

/// "7d", "36h", "90m", "45s" or "0": a whole number with a unit, as seconds.
fn parse_age(text: &str) -> std::result::Result<u64, String> {
    let bad = || format!("compress_idle_after is \"{text}\"; give a number with d, h, m or s (7d)");
    if text == "0" {
        return Ok(0);
    }
    let unit = text.chars().last().ok_or_else(bad)?;
    let seconds: u64 = match unit {
        'd' => 24 * 3600,
        'h' => 3600,
        'm' => 60,
        's' => 1,
        _ => return Err(bad()),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = digits.parse().map_err(|_| bad())?;
    count.checked_mul(seconds).ok_or_else(bad)
}

/// Read `<config>/config.toml` (missing file = defaults). `PODBAG_STORE_LIMIT`,
/// `PODBAG_MEMORY_BUDGET`, `PODBAG_DEFAULT_SUBNET`, `PODBAG_IMAGE_MOUNT`, `PODBAG_KEEPER`,
/// `PODBAG_KEEPER_LINGER_MS`, `PODBAG_IMAGE_COMPRESSION` and `PODBAG_COMPRESS_IDLE_AFTER`
/// override it.
pub fn load(dirs: &Dirs) -> Result<Config> {
    let path = dirs.config.join("config.toml");
    let mut config = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|mut e| {
            e.what = format!("invalid settings in {}", path.display());
            e
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            return Err(Error::io(
                format!("cannot read {}", path.display()),
                &e,
                "check the file permissions",
            ));
        }
    };
    apply_overrides(&mut config, |k| std::env::var(k).ok())?;
    Ok(config)
}

/// Set `key = "value"` in `<config>/config.toml`, which is made when missing. The line that
/// sets `key` is replaced (each one, when there are several), or the line is added at the
/// end; every other line stays as it is. A value [`parse`] refuses changes nothing. The
/// file is replaced at once: a reader sees the old text or the new one.
pub fn set_value(dirs: &Dirs, key: &str, value: &str) -> Result<()> {
    let path = dirs.config.join("config.toml");
    let old = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Error::io(
                format!("cannot read {}", path.display()),
                &e,
                "check the file permissions",
            ));
        }
    };
    let setting = format!("{key} = \"{value}\"");
    let sets_key = |line: &str| {
        strip_comment(line)
            .split_once('=')
            .is_some_and(|(k, _)| k.trim() == key)
    };
    let mut found = false;
    let mut text = String::new();
    for line in old.lines() {
        if sets_key(line) {
            found = true;
            text.push_str(&setting);
        } else {
            text.push_str(line);
        }
        text.push('\n');
    }
    if !found {
        text.push_str(&setting);
        text.push('\n');
    }
    // The value alone first, so its error does not point at a line of the file.
    parse(&setting).map_err(|e| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid {key}"),
            e.why.trim_start_matches("line 1: "),
            "use a valid value",
        )
    })?;
    parse(&text).map_err(|mut e| {
        e.what = format!("invalid settings in {}", path.display());
        e
    })?;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dirs.config)?;
        // A new file gets the mode any new file gets (the umask applies), not 0600.
        #[cfg(unix)]
        let mut tmp = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666))
            .tempfile_in(&dirs.config)?;
        #[cfg(not(unix))]
        let mut tmp = tempfile::NamedTempFile::new_in(&dirs.config)?;
        std::io::Write::write_all(&mut tmp, text.as_bytes())?;
        // The file keeps the mode it had: this replaces the user's whole file.
        match std::fs::metadata(&path) {
            Ok(old) => tmp.as_file().set_permissions(old.permissions())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // On disk before it takes the old file's place: after a crash an empty file would
        // read as all defaults, and every other setting would be lost without an error.
        tmp.as_file().sync_all()?;
        tmp.persist(&path)?;
        Ok(())
    };
    write().map_err(|e| {
        Error::io(
            format!("cannot write {}", path.display()),
            &e,
            "check the folder's permissions",
        )
    })
}

/// Seconds as `compress_idle_after` writes them: the largest unit that divides them (7d).
pub fn format_age(seconds: u64) -> String {
    match seconds {
        0 => "0".to_string(),
        s if s % (24 * 3600) == 0 => format!("{}d", s / (24 * 3600)),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The environment variables that override the file.
fn apply_overrides(config: &mut Config, var: impl Fn(&str) -> Option<String>) -> Result<()> {
    if let Some(v) = var("PODBAG_STORE_LIMIT") {
        config.store_limit = parse_size(&v)?;
    }
    if let Some(v) = var("PODBAG_MEMORY_BUDGET") {
        config.memory_budget = parse_budget(&v)?;
    }
    if let Some(v) = var("PODBAG_DEFAULT_SUBNET") {
        crate::network::parse_cidr(&v)?;
        config.default_subnet = v;
    }
    if let Some(v) = var("PODBAG_IMAGE_MOUNT") {
        config.image_mount = ImageMount::parse(&v).map_err(|why| {
            Error::new(
                ErrorKind::InvalidInput,
                "invalid PODBAG_IMAGE_MOUNT",
                why,
                "set it to auto or hardlink, or unset it",
            )
        })?;
    }
    if let Some(v) = var("PODBAG_IMAGE_COMPRESSION") {
        config.image_compression = ImageCompression::parse(&v).map_err(|why| {
            Error::new(
                ErrorKind::InvalidInput,
                "invalid PODBAG_IMAGE_COMPRESSION",
                why,
                "set it to off or idle, or unset it",
            )
        })?;
    }
    if let Some(v) = var("PODBAG_COMPRESS_IDLE_AFTER") {
        config.compress_idle_after = parse_age(&v).map_err(|why| {
            Error::new(
                ErrorKind::InvalidInput,
                "invalid PODBAG_COMPRESS_IDLE_AFTER",
                why,
                "set it like 7d, 36h, 90m, 45s or 0, or unset it",
            )
        })?;
    }
    let invalid = |name: &str, why: String| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid {name}"),
            why,
            "set it to 1 or 0, or unset it",
        )
    };
    if let Some(v) = var("PODBAG_KEEPER") {
        config.keeper = parse_switch(&v).map_err(|why| invalid("PODBAG_KEEPER", why))?;
    }
    if let Some(v) = var("PODBAG_KEEPER_LINGER_MS") {
        config.keeper_linger_ms = v.parse().map_err(|_| {
            invalid(
                "PODBAG_KEEPER_LINGER_MS",
                format!("{v} is not a number of milliseconds"),
            )
        })?;
    }
    Ok(())
}

/// Parse `key = value` lines. `#` starts a comment. Values are quoted strings or integers.
pub fn parse(text: &str) -> Result<Config> {
    let mut config = Config::default();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let bad = |why: String| {
            Error::new(
                ErrorKind::InvalidInput,
                "invalid settings",
                format!("line {}: {why}", n + 1),
                "fix the line, or delete config.toml to use the defaults",
            )
        };
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| bad("expected `name = value`".into()))?;
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "store_limit" => config.store_limit = parse_size(value).map_err(|e| bad(e.why))?,
            "memory_budget" => {
                config.memory_budget = parse_budget(value).map_err(|e| bad(e.why))?
            }
            "default_subnet" => {
                crate::network::parse_cidr(value).map_err(|e| bad(e.why))?;
                config.default_subnet = value.to_string();
            }
            "image_mount" => config.image_mount = ImageMount::parse(value).map_err(bad)?,
            "image_compression" => {
                config.image_compression = ImageCompression::parse(value).map_err(bad)?
            }
            "compress_idle_after" => config.compress_idle_after = parse_age(value).map_err(bad)?,
            "keeper" => config.keeper = parse_switch(value).map_err(bad)?,
            "keeper_linger" => {
                config.keeper_linger_ms = value
                    .parse()
                    .map_err(|_| bad(format!("{value} is not a number of milliseconds")))?
            }
            other => return Err(bad(format!("unknown setting {other}"))),
        }
    }
    Ok(config)
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

/// "1024", "500MB", "10GiB", "1.5GB": decimal units are powers of 1000, binary ones of 1024.
pub fn parse_size(s: &str) -> Result<u64> {
    let bad = || {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid size {s:?}"),
            "a size is a number with an optional unit: B, kB, MB, GB, KiB, MiB or GiB",
            "write it like 10GiB or 500MB",
        )
    };
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, unit) = (&s[..split], s[split..].trim());
    let value: f64 = number.parse().map_err(|_| bad())?;
    let multiplier: u64 = match unit {
        "" | "B" => 1,
        "kB" | "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return Err(bad()),
    };
    Ok((value * multiplier as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("500MB").unwrap(), 500_000_000);
        assert_eq!(parse_size("10GiB").unwrap(), 10 << 30);
        assert_eq!(parse_size("1.5GB").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("3 MB").unwrap(), 3_000_000);
        assert_eq!(parse_size("64KiB").unwrap(), 65536);
        for bad in ["", "10XB", "-1", "GB"] {
            assert_eq!(
                parse_size(bad).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn reads_store_limit_and_comments() {
        let c = parse("# my settings\n\nstore_limit = \"2GB\"  # cap\n").unwrap();
        assert_eq!(c.store_limit, 2_000_000_000);
        assert_eq!(parse("store_limit = 4096").unwrap().store_limit, 4096);
    }

    #[test]
    fn rejects_unknown_settings() {
        let e = parse("store_limt = \"1GB\"").unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(e.why.contains("store_limt"), "{}", e.why);
    }

    #[test]
    fn reads_memory_budget() {
        let budget = |text: &str| parse(text).unwrap().memory_budget;
        assert_eq!(budget("memory_budget = \"25%\""), Budget::Percent(25));
        assert_eq!(budget("memory_budget = \"0\""), Budget::Unlimited);
        assert_eq!(budget(""), Budget::Percent(50));
    }

    #[test]
    fn reads_default_subnet() {
        assert_eq!(parse("").unwrap().default_subnet, "10.77.0.0/16");
        let c = parse("default_subnet = \"172.31.0.0/16\"").unwrap();
        assert_eq!(c.default_subnet, "172.31.0.0/16");
        assert!(parse("default_subnet = \"10.0.0.1/16\"").is_err());
    }

    #[test]
    fn keeper_settings() {
        let c = parse("").unwrap();
        assert_eq!((c.keeper, c.keeper_linger_ms), (KEEPER_DEFAULT, 10_000));
        let c = parse("keeper = false\nkeeper_linger = 2500").unwrap();
        assert_eq!((c.keeper, c.keeper_linger_ms), (false, 2500));
        assert!(parse("keeper = true").unwrap().keeper);
        assert!(parse("keeper = \"maybe\"").is_err());
        let mut c = Config::default();
        let env = |k: &str| match k {
            "PODBAG_KEEPER" => Some("1".to_string()),
            "PODBAG_KEEPER_LINGER_MS" => Some("300".to_string()),
            _ => None,
        };
        apply_overrides(&mut c, env).unwrap();
        assert_eq!((c.keeper, c.keeper_linger_ms), (true, 300));
        let off = |k: &str| (k == "PODBAG_KEEPER").then(|| "0".to_string());
        apply_overrides(&mut c, off).unwrap();
        assert!(!c.keeper);
        let bad = |k: &str| (k == "PODBAG_KEEPER").then(|| "perhaps".to_string());
        assert!(apply_overrides(&mut c, bad).is_err());
    }

    #[test]
    fn image_mount_setting() {
        assert_eq!(parse("").unwrap().image_mount, ImageMount::Auto);
        let mount = |text: &str| parse(text).unwrap().image_mount;
        assert_eq!(mount("image_mount = \"hardlink\""), ImageMount::Hardlink);
        assert_eq!(mount("image_mount = \"auto\""), ImageMount::Auto);
        let e = parse("image_mount = \"overlay\"").unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(
            e.why.contains("auto") && e.why.contains("hardlink"),
            "{}",
            e.why
        );
        let mut c = parse("image_mount = \"auto\"").unwrap();
        apply_overrides(&mut c, |k| {
            (k == "PODBAG_IMAGE_MOUNT").then(|| "hardlink".to_string())
        })
        .unwrap();
        assert_eq!(c.image_mount, ImageMount::Hardlink);
        assert!(
            apply_overrides(&mut c, |k| (k == "PODBAG_IMAGE_MOUNT").then(|| "x".into())).is_err()
        );
    }

    #[test]
    fn compression_is_off_by_default() {
        let c = parse("").unwrap();
        assert_eq!(
            (c.image_compression, c.compress_idle_after),
            (ImageCompression::Off, 604_800)
        );
    }

    #[test]
    fn reads_compression_settings() {
        let c = parse("image_compression = \"idle\"\ncompress_idle_after = \"36h\"\n").unwrap();
        assert_eq!(
            (c.image_compression, c.compress_idle_after),
            (ImageCompression::Idle, 129_600)
        );
    }

    #[test]
    fn rejects_bad_compression_settings() {
        assert!(parse("image_compression = \"zstd\"").is_err());
        assert!(parse("compress_idle_after = \"7 days\"").is_err());
    }

    #[test]
    fn parses_ages() {
        assert_eq!(parse_age("7d").unwrap(), 604_800);
        assert_eq!(parse_age("36h").unwrap(), 129_600);
        assert_eq!(parse_age("90m").unwrap(), 5_400);
        assert_eq!(parse_age("45s").unwrap(), 45);
        assert_eq!(parse_age("0").unwrap(), 0);
        for bad in [
            "",
            "d",
            "7",
            "7 d",
            "7w",
            "-1d",
            "1.5h",
            "99999999999999999999d",
        ] {
            assert!(parse_age(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_age("7 days").unwrap_err(),
            "compress_idle_after is \"7 days\"; give a number with d, h, m or s (7d)"
        );
    }

    #[test]
    fn compression_environment_overrides() {
        let mut c = Config::default();
        let env = |k: &str| match k {
            "PODBAG_IMAGE_COMPRESSION" => Some("idle".to_string()),
            "PODBAG_COMPRESS_IDLE_AFTER" => Some("0".to_string()),
            _ => None,
        };
        apply_overrides(&mut c, env).unwrap();
        assert_eq!(
            (c.image_compression, c.compress_idle_after),
            (ImageCompression::Idle, 0)
        );
        let bad = |k: &str| (k == "PODBAG_IMAGE_COMPRESSION").then(|| "zstd".to_string());
        assert!(apply_overrides(&mut c, bad).is_err());
        let bad = |k: &str| (k == "PODBAG_COMPRESS_IDLE_AFTER").then(|| "soon".to_string());
        assert!(apply_overrides(&mut c, bad).is_err());
    }

    #[test]
    fn formats_ages_as_they_are_written() {
        for (seconds, text) in [
            (604_800, "7d"),
            (129_600, "36h"),
            (5_400, "90m"),
            (45, "45s"),
            (0, "0"),
        ] {
            assert_eq!(format_age(seconds), text);
            assert_eq!(parse_age(text).unwrap(), seconds);
        }
    }

    fn dirs_with_config(config: std::path::PathBuf) -> Dirs {
        Dirs {
            data: config.join("data"),
            runtime: config.join("run"),
            config,
        }
    }

    #[test]
    fn set_value_replaces_the_line_and_keeps_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let dirs = dirs_with_config(root.path().to_path_buf());
        let file = root.path().join("config.toml");
        std::fs::write(
            &file,
            "# my settings\nstore_limit = \"2GB\"\nimage_compression = \"off\"\n",
        )
        .unwrap();
        set_value(&dirs, "image_compression", "idle").unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# my settings\n"), "{text}");
        assert!(text.contains("store_limit = \"2GB\"\n"), "{text}");
        let lines = text.lines().filter(|l| l.contains("image_compression"));
        assert_eq!(lines.collect::<Vec<_>>(), ["image_compression = \"idle\""]);
        // Parsed directly: `load` would apply the environment overrides.
        let c = parse(&text).unwrap();
        assert_eq!(c.image_compression, ImageCompression::Idle);
        assert_eq!(c.store_limit, 2_000_000_000);
    }

    #[test]
    fn set_value_appends_a_missing_key_and_creates_the_file() {
        let root = tempfile::tempdir().unwrap();
        let dirs = dirs_with_config(root.path().join("no").join("such"));
        set_value(&dirs, "compress_idle_after", "36h").unwrap();
        let text = std::fs::read_to_string(dirs.config.join("config.toml")).unwrap();
        assert_eq!(parse(&text).unwrap().compress_idle_after, 129_600);
    }

    /// The writer replaces the user's whole file: it keeps the mode the file had.
    #[cfg(unix)]
    #[test]
    fn set_value_keeps_the_mode_of_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dirs = dirs_with_config(root.path().to_path_buf());
        let file = root.path().join("config.toml");
        std::fs::write(&file, "store_limit = \"2GB\"\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        set_value(&dirs, "image_compression", "idle").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o640);
    }

    #[test]
    fn set_value_refuses_an_invalid_value() {
        let root = tempfile::tempdir().unwrap();
        let dirs = dirs_with_config(root.path().to_path_buf());
        let file = root.path().join("config.toml");
        let before = "image_compression = \"off\"\n";
        std::fs::write(&file, before).unwrap();
        let e = set_value(&dirs, "image_compression", "zstd").unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn defaults_to_10_gib() {
        assert_eq!(parse("").unwrap().store_limit, 10 << 30);
    }
}
