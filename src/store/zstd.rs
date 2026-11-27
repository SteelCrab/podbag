//! zstd decoding of every frame in a stream. Layers such as zstd:chunked are many frames,
//! and ruzstd's StreamingDecoder stops after the first one.
//!
//! It also holds the cold object format: one skippable header frame, then zstd frames.

use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use ruzstd::encoding::{CompressionLevel, compress_to_vec};
use std::io::{BufRead, Read, Write};

/// Decodes all frames of a zstd stream in order, skipping skippable frames.
pub struct ZstdFrames<R: BufRead> {
    source: R,
    decoder: FrameDecoder,
    in_frame: bool,
}

impl<R: BufRead> ZstdFrames<R> {
    pub fn new(source: R) -> Self {
        ZstdFrames {
            source,
            decoder: FrameDecoder::new(),
            in_frame: false,
        }
    }
}

impl<R: BufRead> Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.in_frame {
                // Same steps as ruzstd's StreamingDecoder, for one frame.
                while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                    self.decoder
                        .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                        .map_err(std::io::Error::other)?;
                }
                let n = self.decoder.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.in_frame = false;
            }
            if self.source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            match self.decoder.init(&mut self.source) {
                Ok(()) => self.in_frame = true,
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    std::io::copy(
                        &mut (&mut self.source).take(u64::from(length)),
                        &mut std::io::sink(),
                    )?;
                }
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }
    }
}

/// Magic of the skippable frame that starts a cold object.
const COLD_MAGIC: u32 = 0x184D_2A50;
/// Payload of the cold header: version, size, flags, mtime.
const COLD_PAYLOAD_LEN: u32 = 18;
const COLD_VERSION: u8 = 1;
const COLD_FLAG_MTIME: u8 = 1;
/// The source is cut into chunks of this size. Each chunk is one zstd frame.
const COLD_CHUNK: usize = 1 << 20;

/// Facts about the original file, kept in the header of a cold object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdHeader {
    pub size: u64,
    pub mtime: Option<u64>,
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

fn take<const N: usize>(r: &mut dyn Read) -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Writes a cold object: a header frame, then `size` bytes of `src` as zstd frames.
/// Returns the number of bytes written. Fails if `src` does not hold exactly `size` bytes.
pub fn write_cold(
    src: &mut dyn Read,
    size: u64,
    mtime: Option<u64>,
    out: &mut dyn Write,
) -> std::io::Result<u64> {
    let mut header = Vec::with_capacity(8 + COLD_PAYLOAD_LEN as usize);
    header.extend_from_slice(&COLD_MAGIC.to_le_bytes());
    header.extend_from_slice(&COLD_PAYLOAD_LEN.to_le_bytes());
    header.push(COLD_VERSION);
    header.extend_from_slice(&size.to_le_bytes());
    header.push(if mtime.is_some() { COLD_FLAG_MTIME } else { 0 });
    header.extend_from_slice(&mtime.unwrap_or(0).to_le_bytes());
    out.write_all(&header)?;
    let mut written = header.len() as u64;

    let mut buf = vec![0u8; COLD_CHUNK.min(usize::try_from(size).unwrap_or(usize::MAX))];
    let mut left = size;
    while left > 0 {
        let n = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        src.read_exact(&mut buf[..n])?;
        left -= n as u64;
        // Keep the smaller one, so data that does not compress grows only a little.
        let packed = compress_to_vec(&buf[..n], CompressionLevel::Fastest);
        let raw = compress_to_vec(&buf[..n], CompressionLevel::Uncompressed);
        let frame = if packed.len() <= raw.len() {
            packed
        } else {
            raw
        };
        out.write_all(&frame)?;
        written += frame.len() as u64;
    }
    if src.read(&mut [0u8; 1])? != 0 {
        return Err(invalid("source is longer than the given size"));
    }
    Ok(written)
}

/// Reads the header of a cold object. Fails if the stream does not start with one.
pub fn read_cold_header(r: &mut dyn Read) -> std::io::Result<ColdHeader> {
    let magic = u32::from_le_bytes(take(r)?);
    let len = u32::from_le_bytes(take(r)?);
    if magic != COLD_MAGIC || len != COLD_PAYLOAD_LEN {
        return Err(invalid("not a cold object header"));
    }
    let [version] = take(r)?;
    if version != COLD_VERSION {
        return Err(invalid("unknown cold object version"));
    }
    let size = u64::from_le_bytes(take(r)?);
    let [flags] = take(r)?;
    let mtime = u64::from_le_bytes(take(r)?);
    Ok(ColdHeader {
        size,
        mtime: (flags & COLD_FLAG_MTIME != 0).then_some(mtime),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn decodes_every_frame_and_skips_skippable_ones() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let mut data = compress_to_vec(&b"first frame, "[..], CompressionLevel::Fastest);
        // A skippable frame: magic 0x184D2A50 (little endian), length 3, payload.
        data.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, b'x', b'y', b'z']);
        data.extend(compress_to_vec(
            &b"second frame"[..],
            CompressionLevel::Fastest,
        ));
        let mut out = String::new();
        ZstdFrames::new(std::io::BufReader::new(data.as_slice()))
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "first frame, second frame");
    }

    #[test]
    fn garbage_is_an_error() {
        let mut out = Vec::new();
        assert!(
            ZstdFrames::new(&b"not zstd at all"[..])
                .read_to_end(&mut out)
                .is_err()
        );
    }

    #[test]
    fn cold_round_trip_keeps_contents_size_and_time() {
        let data: Vec<u8> = (0..(3 << 20) + 5).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        let written = write_cold(
            &mut &data[..],
            data.len() as u64,
            Some(1_700_000_000),
            &mut out,
        )
        .unwrap();
        assert_eq!(written, out.len() as u64);
        assert!(out.len() < data.len());
        let h = read_cold_header(&mut &out[..]).unwrap();
        assert_eq!((h.size, h.mtime), (data.len() as u64, Some(1_700_000_000)));
        let mut back = Vec::new();
        ZstdFrames::new(std::io::BufReader::new(&out[..]))
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn cold_header_without_time() {
        let mut out = Vec::new();
        write_cold(&mut &b"abc"[..], 3, None, &mut out).unwrap();
        assert_eq!(read_cold_header(&mut &out[..]).unwrap().mtime, None);
    }

    #[test]
    fn cold_short_source_is_an_error() {
        let mut out = Vec::new();
        assert!(write_cold(&mut &b"12345"[..], 10, None, &mut out).is_err());
    }

    #[test]
    fn cold_long_source_is_an_error() {
        let mut out = Vec::new();
        assert!(write_cold(&mut &b"12345"[..], 3, None, &mut out).is_err());
    }

    #[test]
    fn a_file_without_the_header_is_not_a_cold_object() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let plain = compress_to_vec(&b"x"[..], CompressionLevel::Fastest);
        assert!(read_cold_header(&mut &plain[..]).is_err());
    }

    #[test]
    fn incompressible_data_grows_by_at_most_a_little() {
        // xorshift64: cheap pseudo-random bytes that do not compress.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let data: Vec<u8> = (0..2 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect();
        let mut out = Vec::new();
        write_cold(&mut &data[..], data.len() as u64, None, &mut out).unwrap();
        assert!(out.len() <= data.len() + 1024);
        let mut back = Vec::new();
        ZstdFrames::new(std::io::BufReader::new(&out[..]))
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, data);
    }
}
