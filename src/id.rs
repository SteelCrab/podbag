//! Container IDs.

use crate::{Error, ErrorKind, Result};
use std::io::Read;

/// A new random container ID: 64 lowercase hex characters.
pub fn new_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| {
            Error::io(
                "cannot create a container ID",
                &e,
                "check that /dev/urandom is readable",
            )
        })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// True for a full container ID: 64 lowercase hex characters.
pub fn is_valid(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reject anything that is not a full container ID. IDs become path components,
/// so this check keeps values such as ".." away from the filesystem.
pub fn check(id: &str) -> Result<()> {
    if is_valid(id) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!("invalid container ID {id:?}"),
        "a container ID is 64 lowercase hexadecimal characters",
        "use the full container ID",
    ))
}

/// The first 12 characters of an ID, as `docker ps` shows them.
pub fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_64_lowercase_hex_and_unique() {
        let (a, b) = (new_id().unwrap(), new_id().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_ne!(a, b);
        assert_eq!(short_id(&a), &a[..12]);
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn only_full_ids_are_valid() {
        assert!(is_valid(&new_id().unwrap()));
        for bad in [
            "",
            "..",
            "../../..",
            "0123abcd",
            &"A".repeat(64),
            &format!("{}/", "a".repeat(63)),
        ] {
            assert!(!is_valid(bad), "{bad:?} must be rejected");
        }
    }
}
