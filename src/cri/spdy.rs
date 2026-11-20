//! SPDY/3.1, as Kubernetes streams use it (exec, attach, port forward): frames, header
//! blocks compressed by zlib with the SPDY dictionary, and a server session that accepts
//! the client's streams.
//!
//! Flow control is not enforced, as in the Go library the clients use.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};

use super::spdy_dict::DICTIONARY;

pub const SYN_STREAM: u16 = 1;
pub const SYN_REPLY: u16 = 2;
pub const RST_STREAM: u16 = 3;
pub const SETTINGS: u16 = 4;
pub const PING: u16 = 6;
pub const GOAWAY: u16 = 7;
pub const HEADERS: u16 = 8;
pub const WINDOW_UPDATE: u16 = 9;
pub const FLAG_FIN: u8 = 0x01;
const VERSION: u16 = 3;
/// The largest frame this side reads.
const MAX_FRAME: usize = 1 << 24;

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Control {
        kind: u16,
        flags: u8,
        data: Vec<u8>,
    },
    Data {
        stream: u32,
        flags: u8,
        data: Vec<u8>,
    },
}

pub fn read_frame(r: &mut impl Read) -> std::io::Result<Frame> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head)?;
    let flags = head[4];
    let len = u32::from_be_bytes([0, head[5], head[6], head[7]]) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("SPDY frame too long"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    if head[0] & 0x80 != 0 {
        let version = u16::from_be_bytes([head[0] & 0x7f, head[1]]);
        if version != VERSION {
            return Err(std::io::Error::other(format!("SPDY version {version}")));
        }
        Ok(Frame::Control {
            kind: u16::from_be_bytes([head[2], head[3]]),
            flags,
            data,
        })
    } else {
        Ok(Frame::Data {
            stream: u32::from_be_bytes([head[0], head[1], head[2], head[3]]),
            flags,
            data,
        })
    }
}

pub fn control(kind: u16, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&(0x8000 | VERSION).to_be_bytes());
    out.extend_from_slice(&kind.to_be_bytes());
    out.push(flags);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(data);
    out
}

pub fn data(stream: u32, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
    out.push(flags);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(data);
    out
}

/// Header pairs, names in lower case. A value with several parts holds them NUL-separated.
pub type Headers = Vec<(String, String)>;

/// The uncompressed form of a header block.
pub fn header_block(headers: &[(&str, &str)]) -> Vec<u8> {
    let mut out = (headers.len() as u32).to_be_bytes().to_vec();
    for (k, v) in headers {
        out.extend_from_slice(&(k.len() as u32).to_be_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u32).to_be_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out
}

pub fn parse_header_block(b: &[u8]) -> Option<Headers> {
    let mut at = 0;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(at..at + n)?;
        at += n;
        Some(s)
    };
    let u32_at = |s: &[u8]| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize;
    let count = u32_at(take(4)?);
    let mut out = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let n = u32_at(take(4)?);
        let name = String::from_utf8_lossy(take(n)?).to_ascii_lowercase();
        let n = u32_at(take(4)?);
        let value = String::from_utf8_lossy(take(n)?).into_owned();
        out.push((name, value));
    }
    Some(out)
}

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

/// This side's header compression: one zlib stream for the connection, each block in
/// stored (uncompressed) deflate blocks. The header names the dictionary, as the reader
/// expects, though nothing refers to it.
#[derive(Default)]
pub struct Compressor {
    started: bool,
}

impl Compressor {
    pub fn compress(&mut self, block: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(block.len() + 16);
        if !self.started {
            // CMF 0x78 (deflate, 32 KiB window), FLG 0x20 (a dictionary; 0x7820 % 31 == 0).
            out.extend_from_slice(&[0x78, 0x20]);
            out.extend_from_slice(&adler32(DICTIONARY).to_be_bytes());
            self.started = true;
        }
        for chunk in block.chunks(0xffff) {
            // BFINAL 0, BTYPE 00, then padding to the byte.
            out.push(0);
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        // An empty stored block, as a sync flush ends with: Go's reader hands the block's
        // bytes out when it meets it, without reading on (past the frame).
        out.extend_from_slice(&[0, 0, 0, 0xff, 0xff]);
        out
    }
}

/// The other side's header compression: one zlib stream for the connection, with the
/// dictionary, flushed after each block.
pub struct Decompressor {
    state: Box<DecompressorOxide>,
    /// What came out so far (the window), the dictionary first.
    window: Vec<u8>,
    /// The zlib header bytes still to skip.
    header_left: usize,
}

impl Default for Decompressor {
    fn default() -> Self {
        Decompressor {
            state: Box::default(),
            window: DICTIONARY.to_vec(),
            header_left: 6,
        }
    }
}

impl Decompressor {
    pub fn decompress(&mut self, mut input: &[u8]) -> std::io::Result<Vec<u8>> {
        let bad = |why: &str| std::io::Error::other(format!("SPDY header block: {why}"));
        if self.header_left > 0 {
            if self.header_left == 6 {
                let cmf = *input.first().ok_or_else(|| bad("empty"))?;
                if cmf & 0x0f != 8 {
                    return Err(bad("not deflate"));
                }
            }
            let skip = self.header_left.min(input.len());
            input = &input[skip..];
            self.header_left -= skip;
        }
        let start = self.window.len();
        let flags = inflate_flags::TINFL_FLAG_HAS_MORE_INPUT
            | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        loop {
            let pos = self.window.len();
            self.window.resize(pos + 16 * 1024, 0);
            let (status, used, made) =
                decompress(&mut self.state, input, &mut self.window, pos, flags);
            self.window.truncate(pos + made);
            input = &input[used..];
            match status {
                TINFLStatus::HasMoreOutput => continue,
                TINFLStatus::NeedsMoreInput | TINFLStatus::Done if input.is_empty() => break,
                TINFLStatus::NeedsMoreInput | TINFLStatus::Done => continue,
                _ => return Err(bad("cannot inflate")),
            }
        }
        let out = self.window[start..].to_vec();
        // Keep the last 32 KiB: matches reach no further.
        if self.window.len() > 128 * 1024 {
            let cut = self.window.len() - 32 * 1024;
            self.window.drain(..cut);
        }
        Ok(out)
    }
}

/// A stream the client opened.
pub struct Stream {
    pub id: u32,
    pub headers: Headers,
    /// Its data, until the client ends it (`None`).
    pub data: Receiver<Option<Vec<u8>>>,
}

impl Stream {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The sending side of a session, shared by the threads that write.
pub struct Writer {
    out: Mutex<Box<dyn Write + Send>>,
    compressor: Mutex<Compressor>,
}

impl Writer {
    pub fn new(out: Box<dyn Write + Send>) -> Writer {
        Writer {
            out: Mutex::new(out),
            compressor: Mutex::new(Compressor::default()),
        }
    }

    fn write(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        out.write_all(frame)?;
        out.flush()
    }

    /// Data on a stream, in frames of at most 32 KiB.
    pub fn data(&self, stream: u32, bytes: &[u8]) -> std::io::Result<()> {
        for chunk in bytes.chunks(32 * 1024) {
            self.write(&data(stream, 0, chunk))?;
        }
        Ok(())
    }

    /// End this side of a stream.
    pub fn close(&self, stream: u32) -> std::io::Result<()> {
        self.write(&data(stream, FLAG_FIN, &[]))
    }

    pub fn reply(&self, stream: u32) -> std::io::Result<()> {
        let block = {
            let mut c = self.compressor.lock().unwrap_or_else(|e| e.into_inner());
            c.compress(&header_block(&[]))
        };
        let mut body = (stream & 0x7fff_ffff).to_be_bytes().to_vec();
        body.extend_from_slice(&block);
        self.write(&control(SYN_REPLY, 0, &body))
    }

    pub fn reset(&self, stream: u32, status: u32) -> std::io::Result<()> {
        let mut body = (stream & 0x7fff_ffff).to_be_bytes().to_vec();
        body.extend_from_slice(&status.to_be_bytes());
        self.write(&control(RST_STREAM, 0, &body))
    }

    pub fn goaway(&self, last: u32) -> std::io::Result<()> {
        let mut body = (last & 0x7fff_ffff).to_be_bytes().to_vec();
        body.extend_from_slice(&0u32.to_be_bytes());
        self.write(&control(GOAWAY, 0, &body))
    }
}

/// Read frames from the client until it goes: accept each new stream (a reply, then it goes
/// to `streams`), pass data on, answer pings. Runs on the caller's thread.
pub fn serve(mut input: impl Read, writer: Arc<Writer>, streams: Sender<Stream>) {
    let mut open: HashMap<u32, Sender<Option<Vec<u8>>>> = HashMap::new();
    let mut headers = Decompressor::default();
    while let Ok(frame) = read_frame(&mut input) {
        match frame {
            Frame::Control { kind, flags, data } => match kind {
                SYN_STREAM if data.len() >= 10 => {
                    let id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) & 0x7fff_ffff;
                    let Some(h) = headers
                        .decompress(&data[10..])
                        .ok()
                        .and_then(|b| parse_header_block(&b))
                    else {
                        // The compression state is lost: the session cannot go on.
                        break;
                    };
                    if writer.reply(id).is_err() {
                        break;
                    }
                    let (tx, rx) = channel();
                    if flags & FLAG_FIN != 0 {
                        let _ = tx.send(None);
                    } else {
                        open.insert(id, tx);
                    }
                    if streams
                        .send(Stream {
                            id,
                            headers: h,
                            data: rx,
                        })
                        .is_err()
                    {
                        let _ = writer.reset(id, 7);
                    }
                }
                HEADERS if data.len() >= 4 => {
                    // Keep the compression state in step; the values are not needed.
                    if headers.decompress(&data[4..]).is_err() {
                        break;
                    }
                }
                RST_STREAM if data.len() >= 4 => {
                    let id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) & 0x7fff_ffff;
                    if let Some(tx) = open.remove(&id) {
                        let _ = tx.send(None);
                    }
                }
                PING => {
                    if writer.write(&control(PING, 0, &data)).is_err() {
                        break;
                    }
                }
                GOAWAY => break,
                _ => {}
            },
            Frame::Data {
                stream,
                flags,
                data,
            } => {
                let fin = flags & FLAG_FIN != 0;
                if let Some(tx) = open.get(&stream) {
                    if !data.is_empty() {
                        let _ = tx.send(Some(data));
                    }
                    if fin {
                        let _ = tx.send(None);
                        open.remove(&stream);
                    }
                }
            }
        }
    }
    for (_, tx) in open {
        let _ = tx.send(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dictionary_is_spdy_3s() {
        assert_eq!(DICTIONARY.len(), 1423);
        assert!(DICTIONARY.starts_with(b"\x00\x00\x00\x07options"));
        assert!(DICTIONARY.ends_with(b"utf-,*,enq=0."));
    }

    #[test]
    fn frames_round_trip() {
        let f = control(SYN_REPLY, 1, b"abc");
        assert_eq!(&f[..4], &[0x80, 0x03, 0x00, 0x02]);
        assert_eq!(
            read_frame(&mut &f[..]).unwrap(),
            Frame::Control {
                kind: SYN_REPLY,
                flags: 1,
                data: b"abc".to_vec()
            }
        );
        let d = data(5, FLAG_FIN, b"hi");
        assert_eq!(
            read_frame(&mut &d[..]).unwrap(),
            Frame::Data {
                stream: 5,
                flags: FLAG_FIN,
                data: b"hi".to_vec()
            }
        );
        let mut v2 = control(PING, 0, b"1234");
        v2[1] = 2;
        assert!(read_frame(&mut &v2[..]).is_err());
    }

    #[test]
    fn header_blocks_round_trip_through_both_compressions() {
        let block = header_block(&[("streamType", "stdout"), ("port", "80")]);
        assert_eq!(
            parse_header_block(&block).unwrap(),
            [
                ("streamtype".to_string(), "stdout".to_string()),
                ("port".to_string(), "80".to_string())
            ]
        );
        assert!(parse_header_block(&block[..block.len() - 1]).is_none());
        let mut c = Compressor::default();
        let mut d = Decompressor::default();
        for _ in 0..3 {
            let wire = c.compress(&block);
            assert_eq!(d.decompress(&wire).unwrap(), block);
        }
    }

    /// Blocks as Go's zlib writes them (made with Python's zlib, the same deflate, with the
    /// dictionary and a sync flush after each block): the second refers to the first.
    #[test]
    fn decompresses_blocks_that_use_the_dictionary_and_the_one_before() {
        let first = header_block(&[
            ("streamtype", "error"),
            ("port", "8080"),
            ("requestid", "0"),
        ]);
        let second = header_block(&[("streamtype", "data"), ("port", "8080"), ("requestid", "0")]);
        let mut d = Decompressor::default();
        assert_eq!(d.decompress(GO_FIRST).unwrap(), first);
        assert_eq!(d.decompress(GO_SECOND).unwrap(), second);
    }

    const GO_FIRST: &[u8] = &[
        0x78, 0xbb, 0xe3, 0xc6, 0xa7, 0xc2, 0x02, 0x25, 0x3a, 0x70, 0x3a, 0x2b, 0x29, 0x4a, 0x4d,
        0xcc, 0x85, 0x16, 0x25, 0xac, 0xa9, 0xa0, 0x80, 0x86, 0x14, 0xcb, 0x45, 0xe0, 0x1c, 0x60,
        0x61, 0x60, 0x61, 0x00, 0xa4, 0x39, 0x8b, 0x20, 0xd1, 0x98, 0x09, 0x4a, 0xde, 0x8c, 0x06,
        0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
    ];
    const GO_SECOND: &[u8] = &[
        0xc2, 0xaa, 0x19, 0x54, 0x0e, 0x25, 0x12, 0xd6, 0x0b, 0x00, 0x00, 0x00, 0xff, 0xff,
    ];

    #[test]
    fn adler32_matches_zlibs() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn a_session_accepts_streams_passes_data_and_answers_pings() {
        let mut c = Compressor::default();
        let mut wire = Vec::new();
        let syn = |id: u32, kind: &str, c: &mut Compressor| {
            let mut body = id.to_be_bytes().to_vec();
            body.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            body.extend_from_slice(&c.compress(&header_block(&[("streamType", kind)])));
            control(SYN_STREAM, 0, &body)
        };
        wire.extend(syn(1, "error", &mut c));
        wire.extend(syn(3, "stdin", &mut c));
        wire.extend(data(3, 0, b"input"));
        wire.extend(data(3, FLAG_FIN, b""));
        wire.extend(control(PING, 0, &7u32.to_be_bytes()));
        let out = Arc::new(Mutex::new(Vec::new()));
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = Arc::new(Writer::new(Box::new(Shared(Arc::clone(&out)))));
        let (tx, rx) = channel();
        serve(&wire[..], Arc::clone(&writer), tx);
        let streams: Vec<Stream> = rx.try_iter().collect();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].header("streamtype"), Some("error"));
        assert_eq!(streams[1].id, 3);
        assert_eq!(streams[1].data.recv().unwrap(), Some(b"input".to_vec()));
        assert_eq!(streams[1].data.recv().unwrap(), None);
        // Two replies, then the ping back.
        let sent = out.lock().unwrap().clone();
        let mut r = &sent[..];
        let mut d = Decompressor::default();
        for id in [1u32, 3] {
            let Frame::Control { kind, data, .. } = read_frame(&mut r).unwrap() else {
                panic!("not a control frame");
            };
            assert_eq!(kind, SYN_REPLY);
            assert_eq!(u32::from_be_bytes(data[..4].try_into().unwrap()), id);
            assert_eq!(d.decompress(&data[4..]).unwrap(), header_block(&[]));
        }
        assert_eq!(
            read_frame(&mut r).unwrap(),
            Frame::Control {
                kind: PING,
                flags: 0,
                data: 7u32.to_be_bytes().to_vec()
            }
        );
    }
}
