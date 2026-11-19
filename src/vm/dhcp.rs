//! A DHCP client for one thing: renewing the lease the kernel's `ip=dhcp` took at boot,
//! which it never renews (the Mac's DHCP server leases for an hour).

use std::collections::HashMap;

/// The server refused the lease (DHCPNAK).
#[derive(Debug, PartialEq, Eq)]
pub struct Nak;

const COOKIE: [u8; 4] = [99, 130, 83, 99];

/// A DHCPREQUEST of a renewing client (RFC 2131 4.3.2): its address in `ciaddr`, no
/// requested address and no server identifier. No client identifier either: the kernel
/// sent none, so the server knows the lease by the hardware address.
pub fn request(xid: u32, ip: [u8; 4], mac: [u8; 6]) -> Vec<u8> {
    let mut p = vec![0u8; 240];
    p[0] = 1; // BOOTREQUEST
    p[1] = 1; // Ethernet
    p[2] = 6;
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[12..16].copy_from_slice(&ip);
    p[28..34].copy_from_slice(&mac);
    p[236..240].copy_from_slice(&COOKIE);
    p.extend([53, 1, 3]); // DHCPREQUEST
    p.extend([55, 3, 1, 3, 6]); // subnet mask, router, name servers
    p.push(255);
    p
}

/// The options after the cookie, by code.
fn options(mut data: &[u8]) -> HashMap<u8, Vec<u8>> {
    let mut out = HashMap::new();
    while let [code, rest @ ..] = data {
        match code {
            0 => data = rest,
            255 => break,
            _ => {
                let [len, rest @ ..] = rest else {
                    break;
                };
                let len = usize::from(*len);
                if rest.len() < len {
                    break;
                }
                out.insert(*code, rest[..len].to_vec());
                data = &rest[len..];
            }
        }
    }
    out
}

/// The lease time an answer to `xid` grants (DHCPACK), or [`Nak`]; None for anything else.
pub fn lease_from_reply(p: &[u8], xid: u32) -> Option<Result<u32, Nak>> {
    if p.len() < 240 || p[0] != 2 || p[4..8] != xid.to_be_bytes() || p[236..240] != COOKIE {
        return None;
    }
    let opts = options(&p[240..]);
    match opts.get(&53).map(Vec::as_slice) {
        Some([5]) => Some(Ok(opts
            .get(&51)
            .and_then(|v| <[u8; 4]>::try_from(v.as_slice()).ok())
            .map_or(3600, u32::from_be_bytes))),
        Some([6]) => Some(Err(Nak)),
        _ => None,
    }
}

/// The DHCP server the kernel's client used, from `/proc/net/pnp` (`bootserver a.b.c.d`).
pub fn server_from_pnp(pnp: &str) -> Option<std::net::Ipv4Addr> {
    pnp.lines()
        .find_map(|l| l.strip_prefix("bootserver "))
        .and_then(|a| a.trim().parse().ok())
        .filter(|a: &std::net::Ipv4Addr| !a.is_unspecified())
}

/// Renew eth0's lease with the server the kernel found; returns the lease time granted.
#[cfg(target_os = "linux")]
pub fn renew() -> std::io::Result<u32> {
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::time::{Duration, Instant};
    // One at a time: both would bind port 68.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let ip = eth0_address()?;
    let mac = eth0_mac()?;
    let server = server_from_pnp(&std::fs::read_to_string("/proc/net/pnp").unwrap_or_default())
        .ok_or_else(|| std::io::Error::other("no DHCP server in /proc/net/pnp"))?;
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68))?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut buf = [0u8; 1500];
    for _ in 0..3 {
        let mut xid = [0u8; 4];
        // SAFETY: getrandom fills the buffer it is given.
        unsafe { libc::getrandom(xid.as_mut_ptr().cast(), 4, 0) };
        let xid = u32::from_ne_bytes(xid);
        socket.send_to(&request(xid, ip, mac), SocketAddrV4::new(server, 67))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let Ok((n, _)) = socket.recv_from(&mut buf) else {
                continue;
            };
            match lease_from_reply(&buf[..n], xid) {
                Some(Ok(secs)) => return Ok(secs),
                Some(Err(Nak)) => {
                    return Err(std::io::Error::other(
                        "the DHCP server refused the address (NAK)",
                    ));
                }
                None => {}
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the DHCP server did not answer",
    ))
}

/// eth0's IPv4 address.
#[cfg(target_os = "linux")]
fn eth0_address() -> std::io::Result<[u8; 4]> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: a new socket for the ioctl; ifreq is plain data filled by the kernel.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut req: libc::ifreq = std::mem::zeroed();
        for (d, s) in req.ifr_name.iter_mut().zip(b"eth0\0") {
            *d = *s as libc::c_char;
        }
        if libc::ioctl(fd.as_raw_fd(), libc::SIOCGIFADDR as _, &mut req) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sin = &*(&req.ifr_ifru.ifru_addr as *const libc::sockaddr).cast::<libc::sockaddr_in>();
        Ok(sin.sin_addr.s_addr.to_ne_bytes())
    }
}

/// eth0's MAC address.
#[cfg(target_os = "linux")]
fn eth0_mac() -> std::io::Result<[u8; 6]> {
    let text = std::fs::read_to_string("/sys/class/net/eth0/address")?;
    let bytes: Vec<u8> = text
        .trim()
        .split(':')
        .filter_map(|o| u8::from_str_radix(o, 16).ok())
        .collect();
    <[u8; 6]>::try_from(bytes).map_err(|_| std::io::Error::other("eth0 has no MAC address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x02, 0xb5, 0x0c, 0x11, 0x22, 0x33];
    const IP: [u8; 4] = [192, 168, 64, 7];

    #[test]
    fn a_renewal_request_names_the_client_and_its_address() {
        let p = request(0x1234_5678, IP, MAC);
        assert_eq!(p[0], 1, "BOOTREQUEST");
        assert_eq!(&p[1..3], &[1, 6], "Ethernet, 6-byte addresses");
        assert_eq!(&p[4..8], &0x1234_5678u32.to_be_bytes());
        assert_eq!(&p[12..16], &IP, "ciaddr: a renewing client");
        assert_eq!(&p[16..20], &[0; 4]);
        assert_eq!(&p[28..34], &MAC);
        assert_eq!(&p[236..240], &[99, 130, 83, 99], "the magic cookie");
        let opts = options(&p[240..]);
        assert_eq!(opts.get(&53), Some(&vec![3]), "DHCPREQUEST");
        // RFC 2131 4.3.2: a renewing client names no requested address and no server.
        assert!(!opts.contains_key(&50) && !opts.contains_key(&54));
    }

    #[test]
    fn an_ack_gives_the_lease_time() {
        let ack = reply(0x1234_5678, 5, Some(3600));
        assert_eq!(lease_from_reply(&ack, 0x1234_5678), Some(Ok(3600)));
    }

    #[test]
    fn the_server_comes_from_the_kernels_record() {
        let pnp =
            "#PROTO: DHCP\ndomain localdomain\nnameserver 192.168.64.1\nbootserver 192.168.64.1\n";
        assert_eq!(server_from_pnp(pnp), Some([192, 168, 64, 1].into()));
        assert_eq!(server_from_pnp("bootserver 0.0.0.0\n"), None);
        assert_eq!(server_from_pnp(""), None);
    }

    #[test]
    fn a_nak_or_another_xid_is_no_ack() {
        assert_eq!(lease_from_reply(&reply(1, 6, None), 1), Some(Err(Nak)));
        assert_eq!(lease_from_reply(&reply(2, 5, Some(60)), 1), None);
        assert_eq!(lease_from_reply(&[0; 10], 1), None);
    }

    /// A server's answer (BOOTREPLY) of message type `kind`, with a lease time.
    fn reply(xid: u32, kind: u8, lease: Option<u32>) -> Vec<u8> {
        let mut p = vec![0u8; 240];
        p[0] = 2;
        p[1] = 1;
        p[2] = 6;
        p[4..8].copy_from_slice(&xid.to_be_bytes());
        p[16..20].copy_from_slice(&IP);
        p[236..240].copy_from_slice(&[99, 130, 83, 99]);
        p.extend([53, 1, kind]);
        if let Some(secs) = lease {
            p.extend([51, 4]);
            p.extend(secs.to_be_bytes());
        }
        p.push(255);
        p
    }
}
