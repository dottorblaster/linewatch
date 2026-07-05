//! Incremental-TTL path probe.
//!
//! [`trace`] sends probes with increasing TTL to discover the network path
//! toward a target.  It prefers ICMP echo requests on a raw socket; if raw
//! ICMP is unavailable it falls back to TCP SYN probes toward `target:443`
//! with ICMP Time-Exceeded reception on a raw socket.

use std::io;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, SockAddr, Type};

// ---------------------------------------------------------------------------
// Hop type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Hop {
    pub ttl: u8,
    pub addr: Option<IpAddr>,
    pub rtt: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

const MAX_HOPS: u8 = 30;

/// Trace the path toward `target` with at most `per_hop_timeout` per hop and
/// a total wall-clock budget of `per_hop_timeout * 30`.
///
/// Returns a vector of up to 30 [`Hop`]s.  The last hop is the target on
/// success, or the last router that responded if the path is broken.
pub async fn trace(target: IpAddr, per_hop_timeout: Duration) -> Vec<Hop> {
    // Try ICMP-TTL first.
    match icmp_ttl_trace(target, per_hop_timeout).await {
        Ok(hops) => return hops,
        Err(_) => { /* fall through to TCP */ }
    }

    // Fallback: TCP traceroute with a total time budget.
    let total_budget = per_hop_timeout.saturating_mul(MAX_HOPS as u32);
    tcp_ttl_trace(target, per_hop_timeout, total_budget).await
}

// ---------------------------------------------------------------------------
// ICMP-TTL traceroute (raw socket, CAP_NET_RAW)
// ---------------------------------------------------------------------------

async fn icmp_ttl_trace(target: IpAddr, per_hop_timeout: Duration) -> io::Result<Vec<Hop>> {
    let sock = create_icmp_sender()?;
    sock.set_write_timeout(Some(per_hop_timeout))?;

    // Best-effort receiver for ICMP responses.
    let recv = create_icmp_receiver().ok();

    let mut hops: Vec<Hop> = Vec::with_capacity(MAX_HOPS as usize);
    let dest = SockAddr::from(SocketAddr::new(target, 0));

    for ttl in 1..=MAX_HOPS {
        sock.set_ttl_v4(ttl as u32)?;

        let ident: u16 = rand::random();
        let seq: u16 = ttl as u16;
        let payload = b"linewatch";
        let pkt = build_icmp_echo(ident, seq, payload);

        let send_start = Instant::now();
        sock.send_to(&pkt, &dest)?;

        let hop = match &recv {
            Some(r) => wait_for_icmp_reply(r, target, ident, seq, per_hop_timeout, send_start, ttl),
            None => {
                // No receiver socket — just report the target at the end.
                tokio::time::sleep(per_hop_timeout).await;
                Hop { ttl, addr: None, rtt: None }
            }
        };

        // If we reached the target we're done.
        if hop.addr == Some(target) {
            hops.push(hop);
            return Ok(hops);
        }
        hops.push(hop);
    }

    Ok(hops)
}

/// Wait for one ICMP reply, parse it and return the corresponding Hop.
fn wait_for_icmp_reply(
    sock: &socket2::Socket,
    target: IpAddr,
    ident: u16,
    seq: u16,
    timeout: Duration,
    send_start: Instant,
    ttl: u8,
) -> Hop {
    let deadline = send_start + timeout;
    let mut buf = [mem::MaybeUninit::new(0u8); 512];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Hop { ttl, addr: None, rtt: None };
        }
        if sock.set_read_timeout(Some(remaining)).is_err() {
            return Hop { ttl, addr: None, rtt: None };
        }

        match sock.recv_from(&mut buf) {
            Ok((len, src)) => {
                // SAFETY: recv_from returned `len` valid bytes.
                let data: &[u8] = unsafe { mem::transmute(&buf[..len]) };
                let rtt = send_start.elapsed();

                // Determine where the ICMP header starts.
                let ip_hdr_len = ((data[0] & 0x0F) as usize) * 4;
                if ip_hdr_len + 4 > len {
                    continue; // malformed
                }
                let icmp_type = data[ip_hdr_len];
                let icmp_code = data[ip_hdr_len + 1];

                // Get the source IP.
                let src_ip = src.as_socket().map(|s| s.ip());

                match (icmp_type, icmp_code) {
                    (11, 0) => {
                        // Time Exceeded in transit.
                        // The embedded original IP header + 8 bytes follow.
                        let orig_ip_hdr_len = match data.get(ip_hdr_len + 8) {
                            Some(&b) => ((b & 0x0F) as usize) * 4,
                            None => continue,
                        };
                        let echo_start = ip_hdr_len + 8 + orig_ip_hdr_len;
                        // We embedded (ident, seq) at offset 4, 6 inside the echo.
                        if echo_start + 8 <= len {
                            let echo_ident =
                                u16::from_be_bytes([data[echo_start + 4], data[echo_start + 5]]);
                            let echo_seq =
                                u16::from_be_bytes([data[echo_start + 6], data[echo_start + 7]]);
                            if echo_ident == ident && echo_seq == seq {
                                let addr = src_ip.unwrap_or(target);
                                return Hop {
                                    ttl,
                                    addr: Some(addr),
                                    rtt: Some(rtt),
                                };
                            }
                        }
                        // Not our packet — keep listening.
                    }
                    (0, 0) => {
                        // Echo Reply — verify identifier.
                        let echo_ident =
                            u16::from_be_bytes([data[ip_hdr_len + 4], data[ip_hdr_len + 5]]);
                        let echo_seq =
                            u16::from_be_bytes([data[ip_hdr_len + 6], data[ip_hdr_len + 7]]);
                        if echo_ident == ident && echo_seq == seq {
                            return Hop {
                                ttl,
                                addr: Some(target),
                                rtt: Some(rtt),
                            };
                        }
                        // Not ours — keep listening.
                    }
                    _ => { /* ignore other ICMP types */ }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Hop { ttl, addr: None, rtt: None };
            }
            Err(_) => {
                // Non-recoverable read error.
                return Hop { ttl, addr: None, rtt: None };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TCP-TTL traceroute (fallback)
// ---------------------------------------------------------------------------

async fn tcp_ttl_trace(target: IpAddr, per_hop_timeout: Duration, total_budget: Duration) -> Vec<Hop> {
    // Try to open a raw ICMP receiver so we can see Time-Exceeded messages.
    let icmp_recv = create_icmp_receiver().ok();
    let mut hops: Vec<Hop> = Vec::with_capacity(MAX_HOPS as usize);
    let deadline = Instant::now() + total_budget;

    for ttl in 1..=MAX_HOPS {
        // Stop if we've exhausted the total time budget.
        if Instant::now() >= deadline {
            break;
        }

        let send_start = Instant::now();
        let reached = tcp_probe_ttl(target, ttl, per_hop_timeout).await;

        // The connect result tells us if we reached the target.
        let (addr, rtt) = match reached {
            Ok(()) => {
                // Connection succeeded — target is reachable.
                (Some(target), Some(send_start.elapsed()))
            }
            Err(ref e) if e.kind() == io::ErrorKind::ConnectionRefused
                || e.kind() == io::ErrorKind::ConnectionReset =>
            {
                // Connection was refused/reset — the target received our
                // SYN but no service is listening on 443.  That's still the
                // target.
                (Some(target), Some(send_start.elapsed()))
            }
            Err(_) => {
                // Connect failed — maybe TTL expired.
                // Check the ICMP receiver for a Time-Exceeded.
                let hop_ip = if let Some(ref r) = icmp_recv {
                    drain_icmp_time_exceeded(r, per_hop_timeout, send_start)
                } else {
                    None
                };
                (hop_ip, hop_ip.map(|_| send_start.elapsed()))
            }
        };

        hops.push(Hop { ttl, addr, rtt });

        if addr == Some(target) {
            break;
        }
    }

    hops
}

/// Try to connect to `target:443` with the given IP TTL.
async fn tcp_probe_ttl(target: IpAddr, ttl: u8, per_hop_timeout: Duration) -> io::Result<()> {
    use std::os::unix::io::{FromRawFd, IntoRawFd};

    // Create a socket2 Socket so we can set TTL and non-blocking.
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_ttl_v4(ttl as u32)?;
    sock.set_nonblocking(true)?;

    let addr = SockAddr::from(SocketAddr::new(target, 443));
    let _ = sock.connect(&addr);

    // Convert to tokio TcpStream and wait for the connection to complete.
    let fd = sock.into_raw_fd();
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true)?;
    let tokio_stream = tokio::net::TcpStream::from_std(std_stream)?;

    use tokio::time::timeout as tok_timeout;
    tok_timeout(per_hop_timeout, async move {
        let _ = tokio_stream.writable().await?;
        // Check for a pending connection error.
        match tokio_stream.take_error()? {
            None => Ok(()),
            Some(e) => Err(e),
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "hop timeout"))?
}

/// Drain any ICMP Time-Exceeded messages from the raw socket that arrived
/// since `send_start` and return the first sender IP.
fn drain_icmp_time_exceeded(
    sock: &Socket,
    timeout: Duration,
    send_start: Instant,
) -> Option<IpAddr> {
    let deadline = send_start + timeout;
    let mut buf = [mem::MaybeUninit::new(0u8); 512];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        if sock.set_read_timeout(Some(remaining)).is_err() {
            return None;
        }

        match sock.recv_from(&mut buf) {
            Ok((len, src)) => {
                // SAFETY: recv_from returned `len` valid bytes.
                let data: &[u8] = unsafe { mem::transmute(&buf[..len]) };
                let ip_hdr_len = ((data[0] & 0x0F) as usize) * 4;
                if ip_hdr_len + 4 > len {
                    continue;
                }
                let icmp_type = data[ip_hdr_len];
                let icmp_code = data[ip_hdr_len + 1];
                if icmp_type == 11 && icmp_code == 0 {
                    return src.as_socket().map(|s| s.ip());
                }
                // Ignore other messages.
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut =>
            {
                return None;
            }
            Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// Socket helpers
// ---------------------------------------------------------------------------

/// Create a raw ICMPv4 socket for *sending* echo requests.
/// Returns an error when the user lacks `CAP_NET_RAW`.
fn create_icmp_sender() -> io::Result<Socket> {
    Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
}

/// Create a raw ICMPv4 socket for *receiving* ICMP messages.
/// May fail when the user lacks `CAP_NET_RAW`.
fn create_icmp_receiver() -> io::Result<Socket> {
    Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
}

// ---------------------------------------------------------------------------
// ICMP packet helpers
// ---------------------------------------------------------------------------

fn build_icmp_echo(ident: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(8 + payload.len());
    pkt.push(8); // Type: Echo Request
    pkt.push(0); // Code
    pkt.extend_from_slice(&[0, 0]); // checksum placeholder
    pkt.extend_from_slice(&ident.to_be_bytes());
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(payload);

    let checksum = internet_checksum(&pkt);
    pkt[2..4].copy_from_slice(&checksum.to_be_bytes());
    pkt
}

fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    const HOP_TIMEOUT: Duration = Duration::from_secs(3);

    #[tokio::test]
    async fn trace_loopback() {
        let hops = trace(IpAddr::V4(Ipv4Addr::LOCALHOST), HOP_TIMEOUT).await;
        println!("Trace to 127.0.0.1:");
        for h in &hops {
            println!("  ttl={}  addr={:?}  rtt={:?}", h.ttl, h.addr, h.rtt);
        }
        assert!(!hops.is_empty(), "should have at least one hop");
        let last = hops.last().unwrap();
        assert_eq!(last.addr, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[tokio::test]
    async fn trace_public_target() {
        let hops = trace(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), HOP_TIMEOUT).await;
        println!("Trace to 1.1.1.1:");
        for h in &hops {
            println!("  ttl={}  addr={:?}  rtt={:?}", h.ttl, h.addr, h.rtt);
        }
        assert!(!hops.is_empty(), "should have at least one hop");
        let last = hops.last().unwrap();
        println!("Last hop: {:?}", last);
    }

    #[tokio::test]
    async fn trace_unreachable_target() {
        // 10.255.255.1 is in the reserved space — unlikely to be routed.
        // Use a short hop timeout to keep the test fast.
        let hops = trace(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)), Duration::from_secs(1)).await;
        println!("Trace to 10.255.255.1:");
        for h in &hops {
            println!("  ttl={}  addr={:?}  rtt={:?}", h.ttl, h.addr, h.rtt);
        }
        assert!(!hops.is_empty());
        let last = hops.last().unwrap();
        assert_ne!(last.addr, Some(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1))));
    }
}
