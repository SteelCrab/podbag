use super::{LogLine, Stream};
use crate::Result;
use crate::log_driver::{Address, Driver, Format};
use crate::spec::ContainerSpec;
use serde::{Deserialize, Serialize};
use socket2::{SockAddr, Socket, Type};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";
const DELIVERY_STATUS: &str = "log-delivery.json";
// The supervisor handles signals and IO for several containers on one thread.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(10);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct Forwarder {
    address: SockAddr,
    socket_type: Type,
    format: Record,
    socket: Option<Socket>,
    retry_at: Instant,
    status_path: PathBuf,
    status: DeliveryStatus,
    pending_status: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DeliveryStatus {
    pub dropped: u64,
    pub last_error: String,
}

pub(crate) fn delivery_status(dir: &Path) -> Option<DeliveryStatus> {
    let bytes = std::fs::read(dir.join(DELIVERY_STATUS)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

enum Record {
    Syslog {
        header: SyslogHeader,
        format: Format,
        local: bool,
    },
    Journal(Vec<u8>),
}

struct SyslogHeader {
    facility: u8,
    hostname: String,
    tag: String,
    pid: u32,
}

impl Forwarder {
    pub(super) fn for_container(dir: &Path, spec: &ContainerSpec) -> Result<Option<Self>> {
        let config = &spec.log_config;
        let (address, format) = match config.driver() {
            Driver::JsonFile => return Ok(None),
            Driver::Journald => (
                Address::UnixGram(JOURNAL_SOCKET.into()),
                Record::Journal(journal_metadata(spec, &config.tag(spec)?)),
            ),
            Driver::Syslog => {
                let syslog = config.syslog()?;
                let local = matches!(syslog.address, Address::Unix(_) | Address::UnixGram(_));
                let header = SyslogHeader {
                    facility: syslog.facility,
                    hostname: hostname(),
                    tag: config.tag(spec)?,
                    pid: std::process::id(),
                };
                (
                    syslog.address,
                    Record::Syslog {
                        header,
                        format: syslog.format,
                        local,
                    },
                )
            }
        };
        // The settings parser limits Unix addresses to Linux's pathname size.
        Self::new(dir, address, format).map(Some).map_err(|e| {
            crate::Error::io(
                "cannot configure log delivery",
                &e,
                "check the log socket address",
            )
        })
    }

    fn new(dir: &Path, address: Address, format: Record) -> io::Result<Self> {
        let (address, socket_type) = match address {
            Address::Udp(addr) => (SockAddr::from(addr), Type::DGRAM),
            Address::Tcp(addr) => (SockAddr::from(addr), Type::STREAM),
            Address::UnixGram(path) => (SockAddr::unix(path)?, Type::DGRAM),
            Address::Unix(path) => (SockAddr::unix(path)?, Type::STREAM),
        };
        Ok(Self {
            address,
            socket_type,
            format,
            socket: None,
            retry_at: Instant::now(),
            status_path: dir.join(DELIVERY_STATUS),
            status: delivery_status(dir).unwrap_or_default(),
            pending_status: false,
        })
    }

    pub(super) fn send(&mut self, line: &LogLine) {
        let now = Instant::now();
        if now < self.retry_at {
            self.drop_line();
            return;
        }
        self.flush_status();
        let priority = match line.stream {
            Stream::Stdout => 6,
            Stream::Stderr => 3,
        };
        let msg = line.log.strip_suffix('\n').unwrap_or(&line.log);
        let bytes = match &self.format {
            Record::Syslog {
                header,
                format,
                local,
            } => header.frame(*format, *local, &line.time, priority, msg),
            Record::Journal(metadata) => journal_frame(metadata, priority, &line.time, msg),
        };
        if let Err(error) = self.write(&bytes) {
            self.socket = None;
            self.retry_at = Instant::now() + RETRY_INTERVAL;
            self.status.last_error = error.to_string();
            self.drop_line();
            self.flush_status();
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.socket.is_none() {
            let socket = Socket::new(self.address.domain(), self.socket_type, None)?;
            if self.socket_type == Type::STREAM {
                socket.connect_timeout(&self.address, CONNECT_TIMEOUT)?;
                socket.set_nonblocking(true)?;
            } else {
                socket.set_nonblocking(true)?;
                socket.connect(&self.address)?;
            }
            self.socket = Some(socket);
        }
        let socket = self.socket.as_ref().expect("socket was connected above");
        let sent = socket.send_with_flags(bytes, libc::MSG_NOSIGNAL)?;
        if sent != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "log socket accepted only part of a record",
            ));
        }
        Ok(())
    }

    fn drop_line(&mut self) {
        self.status.dropped = self.status.dropped.saturating_add(1);
        self.pending_status = true;
    }

    pub(super) fn flush_status(&mut self) {
        if !self.pending_status {
            return;
        }
        let temp = self.status_path.with_extension("tmp");
        if let Ok(bytes) = serde_json::to_vec(&self.status) {
            let result = std::fs::write(&temp, bytes)
                .and_then(|()| std::fs::rename(&temp, &self.status_path));
            self.pending_status = result.is_err();
        }
    }
}

fn hostname() -> String {
    let mut bytes = [0u8; 256];
    // SAFETY: the buffer is writable for its full length. Ignore errors and use "-".
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } == 0 {
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        token(&String::from_utf8_lossy(&bytes[..end]), 255)
    } else {
        "-".into()
    }
}

fn token(s: &str, max_len: usize) -> String {
    if s.is_empty() {
        return "-".into();
    }
    s.bytes()
        .take(max_len)
        .map(|b| if b.is_ascii_graphic() { b as char } else { '_' })
        .collect()
}

impl SyslogHeader {
    fn frame(&self, format: Format, local: bool, time: &str, severity: u8, msg: &str) -> Vec<u8> {
        let priority = self.facility * 8 + severity;
        let msg = msg.replace('\0', "\\0");
        let host = &self.hostname;
        let pid = self.pid;
        let app = token(&self.tag, 48);
        match format {
            Format::Rfc5424 => format!(
                "<{priority}>1 {}Z {host} {app} {pid} - - {msg}\n",
                &time[..19]
            ),
            Format::Rfc5424Micro => format!(
                "<{priority}>1 {}Z {host} {app} {pid} - - {msg}\n",
                &time[..26]
            ),
            Format::Rfc3164 => format!(
                "<{priority}>{} {host} {app}[{pid}]: {msg}\n",
                bsd_time(time)
            ),
            Format::Default if local => {
                format!("<{priority}>{} {app}[{pid}]: {msg}\n", bsd_time(time))
            }
            Format::Default => format!("<{priority}>{}Z {host} {app}[{pid}]: {msg}\n", &time[..19]),
        }
        .into_bytes()
    }
}

fn bsd_time(time: &str) -> String {
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month: usize = time[5..7].parse().unwrap_or(1);
    let day = time[8..10].trim_start_matches('0');
    format!("{} {day:>2} {}", months[month - 1], &time[11..19])
}

fn journal_metadata(spec: &ContainerSpec, tag: &str) -> Vec<u8> {
    let mut data = Vec::new();
    for (key, value) in [
        ("CONTAINER_ID", spec.id.get(..12).unwrap_or(&spec.id)),
        ("CONTAINER_ID_FULL", &spec.id),
        ("CONTAINER_NAME", &spec.name),
        ("CONTAINER_TAG", tag),
        ("SYSLOG_IDENTIFIER", tag),
        ("IMAGE_NAME", spec.image_name.as_deref().unwrap_or_default()),
    ] {
        journal_field(&mut data, key, value);
    }
    data
}

fn journal_frame(metadata: &[u8], severity: u8, time: &str, msg: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(metadata.len() + msg.len() + 20);
    data.extend_from_slice(metadata);
    journal_field(&mut data, "PRIORITY", if severity == 3 { "3" } else { "6" });
    journal_field(&mut data, "SYSLOG_TIMESTAMP", time);
    journal_field(&mut data, "MESSAGE", msg);
    data
}

fn journal_field(data: &mut Vec<u8>, key: &str, value: &str) {
    data.extend_from_slice(key.as_bytes());
    if value.contains(['\n', '\0']) {
        data.push(b'\n');
        data.extend_from_slice(&(value.len() as u64).to_le_bytes());
    } else {
        data.push(b'=');
    }
    data.extend_from_slice(value.as_bytes());
    data.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_driver::LogConfig;
    use crate::shim::logbook::{LogWriter, read_logs};
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn journald_receives_metadata_and_binary_message_with_local_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let journal = UnixDatagram::bind(&path).unwrap();
        journal
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut spec = crate::spec::spec_fixture();
        spec.image_name = Some("alpine:latest".into());
        spec.log_config = LogConfig::parse(
            "journald",
            [("tag".into(), "podbag-{{.Name}}".into())].into(),
        )
        .unwrap();
        let metadata = journal_metadata(&spec, &spec.log_config.tag(&spec).unwrap());
        let mut log = LogWriter::open(dir.path(), 1024).unwrap();
        log.forward = Some(
            Forwarder::new(
                dir.path(),
                Address::UnixGram(path.to_str().unwrap().into()),
                Record::Journal(metadata),
            )
            .unwrap(),
        );

        log.write(Stream::Stderr, b"nul\0byte\n").unwrap();
        let mut bytes = [0; 1024];
        let len = journal.recv(&mut bytes).unwrap();
        let bytes = &bytes[..len];
        let prefix = std::str::from_utf8(bytes.split(|b| *b == 0).next().unwrap()).unwrap();
        for field in [
            "CONTAINER_ID=aaaaaaaaaaaa",
            &format!("CONTAINER_ID_FULL={}", spec.id),
            "CONTAINER_NAME=box",
            "CONTAINER_TAG=podbag-box",
            "SYSLOG_IDENTIFIER=podbag-box",
            "IMAGE_NAME=alpine:latest",
            "PRIORITY=3",
        ] {
            assert!(prefix.lines().any(|line| line == field), "{prefix}");
        }
        assert!(prefix.contains("SYSLOG_TIMESTAMP="));
        assert!(bytes.ends_with(b"MESSAGE\n\x08\0\0\0\0\0\0\0nul\0byte\n"));
        assert_eq!(read_logs(dir.path()).unwrap()[0].log, "nul\0byte\n");
        assert!(delivery_status(dir.path()).is_none());
    }

    #[test]
    fn tcp_delivers_a_complete_syslog_record_to_loopback() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;

        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut forward = Forwarder::new(
            dir.path(),
            Address::Tcp(listener.local_addr().unwrap()),
            Record::Syslog {
                header: SyslogHeader {
                    facility: 3,
                    hostname: "host".into(),
                    tag: "box".into(),
                    pid: 12,
                },
                format: Format::Rfc5424Micro,
                local: false,
            },
        )
        .unwrap();
        forward.send(&LogLine {
            log: "tcp-probe\n".into(),
            stream: Stream::Stdout,
            time: "2026-11-27T01:02:03.123456789Z".into(),
        });
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut read = String::new();
        BufReader::new(stream).read_line(&mut read).unwrap();
        assert_eq!(
            read,
            "<30>1 2026-11-27T01:02:03.123456Z host box 12 - - tcp-probe\n"
        );
    }

    #[test]
    fn failed_delivery_leaves_logs_and_recovers_with_next_available_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let mut log = LogWriter::open(dir.path(), 1024).unwrap();
        log.forward = Some(
            Forwarder::new(
                dir.path(),
                Address::UnixGram(path.to_str().unwrap().into()),
                Record::Journal(Vec::new()),
            )
            .unwrap(),
        );

        log.write(Stream::Stdout, b"first\n").unwrap();
        let status = delivery_status(dir.path()).unwrap();
        assert_eq!(status.dropped, 1);
        assert!(!status.last_error.is_empty());
        let journal = UnixDatagram::bind(&path).unwrap();
        journal
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        log.write(Stream::Stdout, b"second\n").unwrap();
        log.forward.as_mut().unwrap().retry_at = Instant::now();
        log.write(Stream::Stdout, b"third").unwrap();
        log.flush_partial().unwrap();

        let mut bytes = [0; 1024];
        let len = journal.recv(&mut bytes).unwrap();
        assert!(bytes[..len].starts_with(b"PRIORITY=6\n"));
        assert!(bytes[..len].ends_with(b"MESSAGE=third\n"));
        assert_eq!(delivery_status(dir.path()).unwrap().dropped, 2);
        let logs = read_logs(dir.path()).unwrap();
        assert_eq!(
            logs.iter()
                .map(|line| line.log.as_str())
                .collect::<Vec<_>>(),
            ["first\n", "second\n", "third"]
        );
    }
}
