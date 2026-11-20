//! HPACK (RFC 7541): the header compression of HTTP/2.
//!
//! The decoder keeps the dynamic table that the peer's encoder fills. The encoder never
//! indexes: it writes the static table's entries by index and everything else as literals,
//! so it keeps no state.

use std::collections::VecDeque;
use std::fmt;
use std::sync::OnceLock;

use super::hpack_tables::{HUFFMAN, STATIC};

/// Octets an entry costs in the dynamic table (RFC 7541 4.1).
const ENTRY_OVERHEAD: usize = 32;

/// A header block that does not follow RFC 7541. It is a connection error
/// (COMPRESSION_ERROR) in HTTP/2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "header compression: {}", self.0)
    }
}

impl std::error::Error for Error {}

/// One header field.
pub type Field = (String, String);

/// Decodes header blocks; one per connection, as it keeps the dynamic table.
pub struct Decoder {
    /// Newest first: index 62 is `table[0]`.
    table: VecDeque<Field>,
    /// Octets the entries take now.
    size: usize,
    /// The table's size now, set by the peer's size updates.
    max_size: usize,
    /// The most the peer may set: what we announced.
    limit: usize,
}

impl Decoder {
    /// A decoder whose table may grow to `limit` octets (our SETTINGS_HEADER_TABLE_SIZE).
    pub fn new(limit: usize) -> Decoder {
        Decoder {
            table: VecDeque::new(),
            size: 0,
            max_size: limit,
            limit,
        }
    }

    /// The fields of one complete header block, in order.
    pub fn decode(&mut self, mut block: &[u8]) -> Result<Vec<Field>, Error> {
        let mut fields = Vec::new();
        while let Some(&first) = block.first() {
            if first & 0x80 != 0 {
                // Indexed field.
                let (index, used) = integer(block, 7)?;
                block = &block[used..];
                fields.push(self.entry(index)?);
            } else if first & 0x40 != 0 {
                // Literal with incremental indexing.
                let (field, used) = self.literal(block, 6)?;
                block = &block[used..];
                self.insert(field.clone());
                fields.push(field);
            } else if first & 0x20 != 0 {
                // Dynamic table size update.
                let (size, used) = integer(block, 5)?;
                block = &block[used..];
                if size > self.limit {
                    return Err(Error("table size above the limit"));
                }
                self.max_size = size;
                self.evict(0);
            } else {
                // Literal without indexing (0000) or never indexed (0001).
                let (field, used) = self.literal(block, 4)?;
                block = &block[used..];
                fields.push(field);
            }
        }
        Ok(fields)
    }

    fn entry(&self, index: usize) -> Result<Field, Error> {
        match index {
            0 => Err(Error("index 0")),
            1..=61 => {
                let (name, value) = STATIC[index - 1];
                Ok((name.to_string(), value.to_string()))
            }
            _ => self
                .table
                .get(index - 62)
                .cloned()
                .ok_or(Error("index past the table")),
        }
    }

    /// A literal field whose name index has a `prefix`-bit integer.
    fn literal(&self, block: &[u8], prefix: u8) -> Result<(Field, usize), Error> {
        let (index, mut used) = integer(block, prefix)?;
        let name = if index == 0 {
            let (name, n) = string(&block[used..])?;
            used += n;
            name
        } else {
            self.entry(index)?.0
        };
        let (value, n) = string(&block[used..])?;
        Ok(((name, value), used + n))
    }

    fn insert(&mut self, field: Field) {
        let size = field.0.len() + field.1.len() + ENTRY_OVERHEAD;
        self.evict(size);
        // An entry larger than the table empties it and is not added (RFC 7541 4.4).
        if size <= self.max_size {
            self.size += size;
            self.table.push_front(field);
        }
    }

    /// Drops the oldest entries until `room` more octets fit.
    fn evict(&mut self, room: usize) {
        while self.size + room > self.max_size {
            match self.table.pop_back() {
                Some((n, v)) => self.size -= n.len() + v.len() + ENTRY_OVERHEAD,
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }
}

/// An integer with an N-bit prefix (RFC 7541 5.1), and the octets it took.
fn integer(block: &[u8], prefix: u8) -> Result<(usize, usize), Error> {
    let max = (1usize << prefix) - 1;
    let first = *block.first().ok_or(Error("integer cut off"))? as usize & max;
    if first < max {
        return Ok((first, 1));
    }
    let mut value = max;
    let mut shift = 0u32;
    for (i, &octet) in block[1..].iter().enumerate() {
        if shift > 28 {
            return Err(Error("integer too large"));
        }
        value += ((octet & 0x7f) as usize) << shift;
        shift += 7;
        if octet & 0x80 == 0 {
            return Ok((value, i + 2));
        }
    }
    Err(Error("integer cut off"))
}

/// A string literal (RFC 7541 5.2), and the octets it took.
fn string(block: &[u8]) -> Result<(String, usize), Error> {
    let huffman = block.first().ok_or(Error("string cut off"))? & 0x80 != 0;
    let (len, used) = integer(block, 7)?;
    let end = used
        .checked_add(len)
        .filter(|&e| e <= block.len())
        .ok_or(Error("string cut off"))?;
    let raw = &block[used..end];
    let bytes = if huffman {
        huffman_decode(raw)?
    } else {
        raw.to_vec()
    };
    Ok((String::from_utf8_lossy(&bytes).into_owned(), end))
}

/// The Huffman code as a binary tree: node `i` has children `tree[i]` (0, 1); a leaf holds
/// `LEAF | symbol`.
const LEAF: u16 = 0x8000;

fn tree() -> &'static Vec<[u16; 2]> {
    static TREE: OnceLock<Vec<[u16; 2]>> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut tree = vec![[0u16; 2]];
        for (symbol, &(code, len)) in HUFFMAN.iter().enumerate() {
            let mut node = 0usize;
            for bit in (0..len).rev() {
                let b = ((code >> bit) & 1) as usize;
                if bit == 0 {
                    tree[node][b] = LEAF | symbol as u16;
                } else {
                    if tree[node][b] == 0 {
                        tree.push([0, 0]);
                        tree[node][b] = (tree.len() - 1) as u16;
                    }
                    node = tree[node][b] as usize;
                }
            }
        }
        tree
    })
}

fn huffman_decode(raw: &[u8]) -> Result<Vec<u8>, Error> {
    let tree = tree();
    let mut out = Vec::with_capacity(raw.len() * 8 / 5);
    let mut node = 0usize;
    // Bits read since the last symbol, and whether they were all ones (padding must be a
    // prefix of the end-of-string code: all ones, at most 7 bits).
    let (mut pending, mut all_ones) = (0u32, true);
    for &octet in raw {
        for bit in (0..8).rev() {
            let b = ((octet >> bit) & 1) as usize;
            pending += 1;
            all_ones &= b == 1;
            let next = tree[node][b];
            if next & LEAF != 0 {
                let symbol = next & !LEAF;
                if symbol == 256 {
                    return Err(Error("end of string inside a string"));
                }
                out.push(symbol as u8);
                node = 0;
                pending = 0;
                all_ones = true;
            } else if next == 0 {
                return Err(Error("bad Huffman code"));
            } else {
                node = next as usize;
            }
        }
    }
    if pending > 7 || !all_ones {
        return Err(Error("bad Huffman padding"));
    }
    Ok(out)
}

/// A header block for `fields`, with no state: static entries by index, the rest as
/// literals without indexing.
pub fn encode(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(name, value) in fields {
        if let Some(i) = STATIC.iter().position(|&(n, v)| n == name && v == value) {
            put_integer(&mut out, 0x80, 7, i + 1);
            continue;
        }
        match STATIC.iter().position(|&(n, _)| n == name) {
            Some(i) => put_integer(&mut out, 0x00, 4, i + 1),
            None => {
                out.push(0x00);
                put_string(&mut out, name);
            }
        }
        put_string(&mut out, value);
    }
    out
}

fn put_integer(out: &mut Vec<u8>, flags: u8, prefix: u8, mut value: usize) {
    let max = (1usize << prefix) - 1;
    if value < max {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max as u8);
    value -= max;
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    put_integer(out, 0x00, 7, s.len());
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri::hpack_examples::EXAMPLES;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn owned(fields: &[(&str, &str)]) -> Vec<Field> {
        fields
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn decodes_the_rfc_examples_in_order() {
        // C.3 and C.4 are requests (table size 4096); C.5 and C.6 responses with 256.
        for group in ["C.3", "C.4", "C.5", "C.6"] {
            let limit = if group < "C.5" { 4096 } else { 256 };
            let mut decoder = Decoder::new(limit);
            for (name, fields, hex) in EXAMPLES.iter().filter(|e| e.0.starts_with(group)) {
                let got = decoder
                    .decode(&unhex(hex))
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(got, owned(fields), "{name}");
            }
        }
    }

    #[test]
    fn encoded_fields_decode_back() {
        let fields = [
            (":status", "200"),
            ("content-type", "application/grpc"),
            ("grpc-status", "0"),
            ("grpc-message", "a message with spaces and ü"),
            (":path", "/runtime.v1.RuntimeService/Version"),
            ("x-long", &"y".repeat(300)),
        ];
        let block = encode(&fields);
        assert_eq!(Decoder::new(4096).decode(&block).unwrap(), owned(&fields));
        // A static entry with its value is a single octet.
        assert_eq!(encode(&[(":status", "200")]), vec![0x88]);
    }

    #[test]
    fn decodes_integers_of_any_size() {
        // RFC 7541 C.1: 10 and 1337 with a 5-bit prefix, 42 at an octet boundary.
        let mut d = Decoder::new(4096);
        // A literal without indexing whose name index is 10 (accept-charset) via 4-bit prefix
        // would not reach the big forms, so use a table size update: 001xxxxx.
        assert!(d.decode(&[0x2a]).is_ok()); // size 10
        assert!(d.decode(&[0x3f, 0x9a, 0x0a]).is_ok()); // size 1337
        assert!(d.decode(&[0x3f, 0xe1, 0x1f]).is_ok()); // size 4096: the limit itself
        assert_eq!(
            d.decode(&[0x3f, 0xe2, 0x1f]),
            Err(Error("table size above the limit"))
        );
    }

    #[test]
    fn rejects_bad_huffman_and_bad_indexes() {
        let mut d = Decoder::new(4096);
        // Indexed field 0 does not exist.
        assert!(d.decode(&[0x80]).is_err());
        // Index 70 is past the static table, and the dynamic table is empty.
        assert!(d.decode(&[0xc6]).is_err());
        // A Huffman string padded with zeros instead of ones ("a" is 00011 → 0x18 with
        // three 0 bits of padding).
        assert!(d.decode(&[0x00, 0x81, 0x18, 0x81, 0x1f]).is_err());
        // A literal that ends in the middle.
        assert!(d.decode(&[0x00, 0x05, b'a']).is_err());
    }

    #[test]
    fn evicts_the_oldest_entries() {
        // A table of 64 octets holds one entry of 32 + 1 + 30 octets; adding a second evicts
        // the first, so index 62 is the newest one.
        let mut d = Decoder::new(64);
        let mut block = vec![0x3f, 0x21]; // size update to 64 (31 + 33)
        for (name, value) in [("a", "x".repeat(30)), ("b", "y".repeat(30))] {
            block.push(0x40);
            block.push(name.len() as u8);
            block.extend(name.as_bytes());
            block.push(value.len() as u8);
            block.extend(value.as_bytes());
        }
        block.push(0xbe); // index 62
        let got = d.decode(&block).unwrap();
        assert_eq!(got.last().unwrap(), &("b".to_string(), "y".repeat(30)));
        assert!(d.decode(&[0xbf]).is_err(), "index 63 was evicted");
    }
}
