//! zstd decoding of every frame in a stream. Layers such as zstd:chunked are many frames,
//! and ruzstd's StreamingDecoder stops after the first one.

use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use std::io::{BufRead, Read};

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
}
