//! How the Mac's CLI talks to the VM (vsock port [`CLI_PORT`]): one JSON line, the
//! [`CliRequest`], then frames both ways: a kind byte, a little-endian u32 length, the data.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// The CLI executor.
pub const CLI_PORT: u32 = 2;
/// The Docker Engine API (`openpod system service`).
pub const API_PORT: u32 = 2375;
/// The forwarder of published ports: a first line `TCP <port>`, answered `OK` once the
/// VM's `127.0.0.1:<port>` is connected (or `ERR …`), then bytes both ways.
pub const FORWARD_PORT: u32 = 3;

/// The argument the kernel passes the VM's /init (after `--` on its command line): only
/// with it is openpod the VM's init.
pub const INIT_ARG: &str = "__vm-init";

/// A command to run in the VM: `openpod <argv…>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliRequest {
    /// The arguments after `openpod`.
    pub argv: Vec<String>,
    /// Environment the command sees (only `OPENPOD_*` variables are sent).
    pub env: Vec<(String, String)>,
    /// The working directory: a Mac path, the same in the VM under /Users.
    pub cwd: String,
    /// Run it under a terminal (the Mac's stdin and stdout are one).
    pub tty: bool,
    pub rows: u16,
    pub cols: u16,
    /// The Mac's clock, seconds and nanoseconds since the epoch: the VM has no clock of
    /// its own to trust.
    pub time: (u64, u32),
}

/// What a frame carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Mac to VM: input for the command.
    Stdin(Vec<u8>),
    /// Mac to VM: the input ended.
    StdinEnd,
    /// Mac to VM: the terminal's new size.
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Mac to VM: a signal for the command (one another process sent the Mac's CLI).
    Signal(i32),
    /// VM to Mac.
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// VM to Mac: the command ended with this code. The last frame.
    Exit(i32),
    /// VM to Mac: this many bytes of input reached the command. The Mac sends input at
    /// most a window ahead, so a command that does not read never fills the VM's memory
    /// or holds up the frames behind it (a signal).
    StdinTaken(u32),
}

/// Frames larger than this are refused (a broken peer, not a command's output).
const MAX_FRAME: u32 = 1 << 20;

impl Frame {
    pub fn write_to(&self, w: &mut impl Write) -> std::io::Result<()> {
        let (kind, data): (u8, std::borrow::Cow<'_, [u8]>) = match self {
            Frame::Stdin(d) => (0, d.into()),
            Frame::StdinEnd => (1, (&[][..]).into()),
            Frame::Resize { rows, cols } => {
                (2, [rows.to_le_bytes(), cols.to_le_bytes()].concat().into())
            }
            Frame::Signal(s) => (3, s.to_le_bytes().to_vec().into()),
            Frame::Stdout(d) => (10, d.into()),
            Frame::Stderr(d) => (11, d.into()),
            Frame::Exit(c) => (12, c.to_le_bytes().to_vec().into()),
            Frame::StdinTaken(n) => (13, n.to_le_bytes().to_vec().into()),
        };
        let mut head = [0u8; 5];
        head[0] = kind;
        head[1..].copy_from_slice(&(data.len() as u32).to_le_bytes());
        w.write_all(&head)?;
        w.write_all(&data)?;
        w.flush()
    }

    /// The next frame; None at a clean end of the stream.
    pub fn read_from(r: &mut impl Read) -> std::io::Result<Option<Frame>> {
        let mut head = [0u8; 5];
        match r.read_exact(&mut head[..1]) {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            other => other?,
        }
        r.read_exact(&mut head[1..])?;
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
        let bad = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string());
        if len > MAX_FRAME {
            return Err(bad("a frame is too large"));
        }
        let mut data = vec![0u8; len as usize];
        r.read_exact(&mut data)?;
        let int = |d: &[u8]| -> std::io::Result<i32> {
            Ok(i32::from_le_bytes(
                d.try_into()
                    .map_err(|_| bad("a frame has the wrong size"))?,
            ))
        };
        Ok(Some(match head[0] {
            0 => Frame::Stdin(data),
            1 => Frame::StdinEnd,
            2 if data.len() == 4 => Frame::Resize {
                rows: u16::from_le_bytes([data[0], data[1]]),
                cols: u16::from_le_bytes([data[2], data[3]]),
            },
            3 => Frame::Signal(int(&data)?),
            10 => Frame::Stdout(data),
            11 => Frame::Stderr(data),
            12 => Frame::Exit(int(&data)?),
            13 => Frame::StdinTaken(int(&data)? as u32),
            _ => return Err(bad("an unknown frame")),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = vec![
            Frame::Stdin(b"input".to_vec()),
            Frame::StdinEnd,
            Frame::Resize {
                rows: 40,
                cols: 120,
            },
            Frame::Signal(15),
            Frame::Stdout(b"out".to_vec()),
            Frame::Stderr(Vec::new()),
            Frame::Exit(-3),
            Frame::StdinTaken(65536),
        ];
        let mut wire = Vec::new();
        for f in &frames {
            f.write_to(&mut wire).unwrap();
        }
        let mut r = wire.as_slice();
        let mut back = Vec::new();
        while let Some(f) = Frame::read_from(&mut r).unwrap() {
            back.push(f);
        }
        assert_eq!(back, frames);
    }

    #[test]
    fn broken_frames_are_errors() {
        // Cut in the middle of the data.
        let mut wire = Vec::new();
        Frame::Stdout(b"0123456789".to_vec())
            .write_to(&mut wire)
            .unwrap();
        wire.truncate(8);
        assert!(Frame::read_from(&mut wire.as_slice()).is_err());
        // Too large, and unknown.
        let huge = [10u8, 0xff, 0xff, 0xff, 0x7f];
        assert!(Frame::read_from(&mut &huge[..]).is_err());
        let unknown = [99u8, 0, 0, 0, 0];
        assert!(Frame::read_from(&mut &unknown[..]).is_err());
    }

    #[test]
    fn requests_round_trip_as_one_line() {
        let req = CliRequest {
            argv: vec!["run".into(), "--rm".into(), "alpine".into()],
            env: vec![("OPENPOD_KEEPER".into(), "0".into())],
            cwd: "/Users/someone/project".into(),
            tty: true,
            rows: 24,
            cols: 80,
            time: (1_700_000_000, 5),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<CliRequest>(&line).unwrap(), req);
    }
}
