//! Published UDP ports on macOS. Each client of a Mac port (its address and port) is a
//! flow: one connection to the VM's forwarder, opened with `UDP <port>`, that carries the
//! flow's datagrams both ways. Each datagram goes as a little-endian u16 length, then the
//! data. A flow ends after [`IDLE`] without a datagram either way. A Mac socket keeps at
//! most [`MAX_FLOWS`] flows: a new client then takes the place of the quietest one.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a flow may stay quiet: 90 seconds, as Docker's own UDP proxy.
pub const IDLE: Duration = Duration::from_secs(90);

/// The flows of one Mac socket, at most.
pub const MAX_FLOWS: usize = 128;

/// The largest datagram: what a u16 length holds, more than UDP carries.
pub const MAX_DATAGRAM: usize = u16::MAX as usize;

/// Send one datagram on the stream.
pub fn write_datagram(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "a datagram is too large")
    })?;
    let mut frame = Vec::with_capacity(2 + data.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(data);
    w.write_all(&frame)?;
    w.flush()
}

/// The next datagram into `buf`, which must hold [`MAX_DATAGRAM`] bytes: its length, or
/// None at a clean end of the stream.
pub fn read_datagram(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    let mut head = [0u8; 2];
    match r.read_exact(&mut head[..1]) {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    }
    r.read_exact(&mut head[1..])?;
    let len = usize::from(u16::from_le_bytes(head));
    let Some(data) = buf.get_mut(..len) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a datagram is larger than the buffer",
        ));
    };
    r.read_exact(data)?;
    Ok(Some(len))
}

/// When a flow last carried a datagram, either way. The thread that carries the replies
/// holds a copy.
#[derive(Debug, Clone)]
pub struct LastUse(Arc<Mutex<Instant>>);

impl LastUse {
    pub fn new(now: Instant) -> LastUse {
        LastUse(Arc::new(Mutex::new(now)))
    }

    pub fn touch(&self, now: Instant) {
        let mut last = self.0.lock().unwrap_or_else(|p| p.into_inner());
        *last = (*last).max(now);
    }

    fn at(&self) -> Instant {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The flows of one Mac socket, by client address.
#[derive(Debug)]
pub struct Flows<F> {
    map: HashMap<SocketAddr, (F, LastUse)>,
    max: usize,
}

impl<F> Flows<F> {
    pub fn new(max: usize) -> Flows<F> {
        Flows {
            map: HashMap::new(),
            max: max.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The client's flow, now used.
    pub fn get(&mut self, client: &SocketAddr, now: Instant) -> Option<&F> {
        let (flow, last) = self.map.get(client)?;
        last.touch(now);
        Some(flow)
    }

    /// Add the client's flow. When the table is full, the quietest flow goes and is
    /// returned, to be closed.
    pub fn add(&mut self, client: SocketAddr, flow: F, last: LastUse) -> Option<F> {
        let evicted = if self.map.len() >= self.max && !self.map.contains_key(&client) {
            let quietest = self
                .map
                .iter()
                .min_by_key(|(_, (_, last))| last.at())
                .map(|(addr, _)| *addr);
            quietest.and_then(|addr| self.remove(&addr))
        } else {
            None
        };
        let replaced = self.map.insert(client, (flow, last)).map(|(f, _)| f);
        evicted.or(replaced)
    }

    pub fn remove(&mut self, client: &SocketAddr) -> Option<F> {
        self.map.remove(client).map(|(flow, _)| flow)
    }

    /// Take out the flows quiet for [`IDLE`] or longer, to be closed.
    pub fn expire(&mut self, now: Instant) -> Vec<F> {
        let idle: Vec<SocketAddr> = self
            .map
            .iter()
            .filter(|(_, (_, last))| now.saturating_duration_since(last.at()) >= IDLE)
            .map(|(addr, _)| *addr)
            .collect();
        idle.iter().filter_map(|addr| self.remove(addr)).collect()
    }

    /// Take out every flow, to be closed.
    pub fn drain(&mut self) -> Vec<F> {
        self.map.drain().map(|(_, (flow, _))| flow).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn datagrams_round_trip() {
        let sent: Vec<Vec<u8>> = vec![
            b"query".to_vec(),
            Vec::new(),
            vec![7u8; MAX_DATAGRAM],
            b"last".to_vec(),
        ];
        let mut wire = Vec::new();
        for d in &sent {
            write_datagram(&mut wire, d).unwrap();
        }
        let mut r = wire.as_slice();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut back = Vec::new();
        while let Some(n) = read_datagram(&mut r, &mut buf).unwrap() {
            back.push(buf[..n].to_vec());
        }
        assert_eq!(back, sent);
    }

    #[test]
    fn broken_datagrams_are_errors() {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        // Too large to send.
        let huge = vec![0u8; MAX_DATAGRAM + 1];
        assert!(write_datagram(&mut Vec::new(), &huge).is_err());
        // Cut in the length, and in the data.
        assert!(read_datagram(&mut &[5u8][..], &mut buf).is_err());
        let mut wire = Vec::new();
        write_datagram(&mut wire, b"0123456789").unwrap();
        wire.truncate(6);
        assert!(read_datagram(&mut wire.as_slice(), &mut buf).is_err());
        // Larger than the buffer.
        let mut wire = Vec::new();
        write_datagram(&mut wire, b"0123456789").unwrap();
        assert!(read_datagram(&mut wire.as_slice(), &mut [0u8; 4]).is_err());
    }

    #[test]
    fn each_client_has_its_own_flow() {
        let now = Instant::now();
        let mut flows = Flows::new(8);
        assert_eq!(flows.add(addr(1000), "a", LastUse::new(now)), None);
        assert_eq!(flows.add(addr(1001), "b", LastUse::new(now)), None);
        assert_eq!(flows.get(&addr(1000), now), Some(&"a"));
        assert_eq!(flows.get(&addr(1001), now), Some(&"b"));
        assert_eq!(flows.get(&addr(1002), now), None);
        assert_eq!(flows.remove(&addr(1000)), Some("a"));
        assert_eq!(flows.get(&addr(1000), now), None);
        assert_eq!(flows.drain(), vec!["b"]);
        assert!(flows.is_empty());
    }

    #[test]
    fn quiet_flows_expire_and_use_keeps_them() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut flows = Flows::new(8);
        flows.add(addr(1000), "used", LastUse::new(start));
        flows.add(addr(1001), "quiet", LastUse::new(start));
        let replies = LastUse::new(start);
        flows.add(addr(1002), "answered", replies.clone());
        assert!(flows.expire(at(60)).is_empty());
        flows.get(&addr(1000), at(60));
        // A reply counts as use too.
        replies.touch(at(80));
        assert_eq!(flows.expire(at(90)), vec!["quiet"]);
        assert_eq!(flows.len(), 2);
        let mut gone = flows.expire(at(171));
        gone.sort();
        assert_eq!(gone, vec!["answered", "used"]);
    }

    #[test]
    fn a_full_table_gives_up_its_quietest_flow() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut flows = Flows::new(2);
        flows.add(addr(1000), "old", LastUse::new(start));
        flows.add(addr(1001), "new", LastUse::new(at(1)));
        flows.get(&addr(1000), at(2));
        assert_eq!(
            flows.add(addr(1002), "third", LastUse::new(at(3))),
            Some("new")
        );
        assert_eq!(flows.len(), 2);
        assert_eq!(flows.get(&addr(1000), at(3)), Some(&"old"));
        assert_eq!(flows.get(&addr(1002), at(3)), Some(&"third"));
    }
}
