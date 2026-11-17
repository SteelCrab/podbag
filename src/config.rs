//! User settings from `<config dir>/config.toml`.

use crate::cgroup::{Budget, parse_budget};
use crate::paths::Dirs;
use crate::{Error, ErrorKind, Result};

/// The default image store limit (design 6.3.4).
pub const DEFAULT_STORE_LIMIT: u64 = 10 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Bytes of image content to keep before the least recently used images are removed.
    pub store_limit: u64,
    /// How much memory all containers may use together (design 6.6).
    pub memory_budget: Budget,
    /// The default network's subnet (design 6.8.2).
    pub default_subnet: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            store_limit: DEFAULT_STORE_LIMIT,
            memory_budget: Budget::default(),
            default_subnet: "10.77.0.0/16".to_string(),
        }
    }
}

/// Read `<config>/config.toml` (missing file = defaults). `OPENPOD_STORE_LIMIT`,
/// `OPENPOD_MEMORY_BUDGET` and `OPENPOD_DEFAULT_SUBNET` override it.
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
    if let Ok(v) = std::env::var("OPENPOD_STORE_LIMIT") {
        config.store_limit = parse_size(&v)?;
    }
    if let Ok(v) = std::env::var("OPENPOD_MEMORY_BUDGET") {
        config.memory_budget = parse_budget(&v)?;
    }
    if let Ok(v) = std::env::var("OPENPOD_DEFAULT_SUBNET") {
        crate::network::parse_cidr(&v)?;
        config.default_subnet = v;
    }
    Ok(config)
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
    fn defaults_to_10_gib() {
        assert_eq!(parse("").unwrap().store_limit, 10 << 30);
    }
}
