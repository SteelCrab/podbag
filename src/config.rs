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
}

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

impl Default for Config {
    fn default() -> Self {
        Config {
            store_limit: DEFAULT_STORE_LIMIT,
            memory_budget: Budget::default(),
            default_subnet: "10.77.0.0/16".to_string(),
            image_mount: ImageMount::Auto,
            keeper: KEEPER_DEFAULT,
            keeper_linger_ms: 10_000,
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

/// Read `<config>/config.toml` (missing file = defaults). `PODBAG_STORE_LIMIT`,
/// `PODBAG_MEMORY_BUDGET`, `PODBAG_DEFAULT_SUBNET`, `PODBAG_IMAGE_MOUNT`, `PODBAG_KEEPER`
/// and `PODBAG_KEEPER_LINGER_MS` override it.
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
    fn defaults_to_10_gib() {
        assert_eq!(parse("").unwrap().store_limit, 10 << 30);
    }
}
