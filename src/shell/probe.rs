//! Probe implementations — each returns a [`ProbeOutcome`] with a hard
//! timeout via `tokio::time::timeout`.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::core::types::{ProbeOutcome, TargetKind};

// ---------------------------------------------------------------------------
// TCP connect probe
// ---------------------------------------------------------------------------

/// Connect to `host:port` via TCP and measure handshake latency.
pub async fn tcp_connect(kind: TargetKind, addr: &str, timeout_dur: Duration) -> ProbeOutcome {
    let start = tokio::time::Instant::now();
    let result = timeout(timeout_dur, TcpStream::connect(addr)).await;
    let elapsed = start.elapsed();

    match result {
        Ok(Ok(_stream)) => ProbeOutcome {
            kind,
            reachable: true,
            rtt: Some(elapsed),
            loss_pct: 0,
        },
        _ => ProbeOutcome {
            kind,
            reachable: false,
            rtt: None,
            loss_pct: 100,
        },
    }
}

// ---------------------------------------------------------------------------
// HTTP check probe
// ---------------------------------------------------------------------------

/// Perform a GET request and check for a 2xx (or 204) status code.
pub async fn http_check(kind: TargetKind, url: &str, timeout_dur: Duration) -> ProbeOutcome {
    let client = reqwest::Client::builder()
        .timeout(timeout_dur)
        .redirect(reqwest::redirect::Policy::none())
        .build();

    let client = match client {
        Ok(c) => c,
        Err(_) => {
            return ProbeOutcome {
                kind,
                reachable: false,
                rtt: None,
                loss_pct: 100,
            };
        }
    };

    let start = tokio::time::Instant::now();
    let result = timeout(timeout_dur, client.get(url).send()).await;
    let elapsed = start.elapsed();

    let reachable = match result {
        Ok(Ok(resp)) => {
            let status = resp.status();
            status.is_success() || status == reqwest::StatusCode::NO_CONTENT
        }
        _ => false,
    };

    ProbeOutcome {
        kind,
        reachable,
        rtt: if reachable { Some(elapsed) } else { None },
        loss_pct: if reachable { 0 } else { 100 },
    }
}

// ---------------------------------------------------------------------------
// DNS check probe
// ---------------------------------------------------------------------------

/// Resolve `name` against a specific upstream nameserver (`host:port`).
pub async fn dns_check(
    kind: TargetKind,
    upstream: &str,
    name: &str,
    timeout_dur: Duration,
) -> ProbeOutcome {
    let sock_addr: SocketAddr = match upstream.parse() {
        Ok(a) => a,
        Err(_) => {
            return ProbeOutcome {
                kind,
                reachable: false,
                rtt: None,
                loss_pct: 100,
            };
        }
    };

    use hickory_resolver::TokioResolver;
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
    use hickory_resolver::net::runtime::TokioRuntimeProvider;

    let ns = NameServerConfig::new(sock_addr.ip(), true, vec![ConnectionConfig::udp()]);
    let config = ResolverConfig::from_parts(None, vec![], vec![ns]);
    let resolver =
        match TokioResolver::builder_with_config(config, TokioRuntimeProvider::default()).build() {
            Ok(r) => r,
            Err(_) => {
                return ProbeOutcome {
                    kind,
                    reachable: false,
                    rtt: None,
                    loss_pct: 100,
                };
            }
        };

    let start = tokio::time::Instant::now();
    let result = timeout(timeout_dur, resolver.lookup_ip(name)).await;
    let elapsed = start.elapsed();

    let reachable = match result {
        Ok(Ok(lookup)) => lookup.iter().next().is_some(),
        _ => false,
    };

    ProbeOutcome {
        kind,
        reachable,
        rtt: if reachable { Some(elapsed) } else { None },
        loss_pct: if reachable { 0 } else { 100 },
    }
}

// ---------------------------------------------------------------------------
// Default gateway detection
// ---------------------------------------------------------------------------

/// Read the system default gateway from `/proc/net/route`.
pub async fn default_gateway() -> Option<Ipv4Addr> {
    // /proc/net/route is a small virtual file — reading it synchronously
    // inside an async fn is fine.
    let contents: String = match fs::read_to_string("/proc/net/route") {
        Ok(c) => c,
        Err(_) => return None,
    };

    for line in contents.lines().skip(1) {
        // Columns: Iface, Destination, Gateway, Flags, ...
        let mut cols = line.split_whitespace();
        let _iface: &str = cols.next()?;
        let dest: &str = cols.next()?;
        let gateway_hex: &str = cols.next()?;

        // Default route has Destination = 00000000
        if dest == "00000000" {
            // The hex value is in host byte order (little-endian on x86).
            // Convert to network byte order for Ipv4Addr::from.
            let raw: u32 = u32::from_str_radix(gateway_hex, 16).ok()?;
            let gw = Ipv4Addr::from(u32::from_be(raw));
            // Skip 0.0.0.0 (no gateway)
            if !gw.is_unspecified() {
                return Some(gw);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// ICMP ping probe
// ---------------------------------------------------------------------------

/// Ping an IPv4 address with 3 echo requests and return the aggregate outcome.
///
/// Uses unprivileged datagram sockets first (`SOCK_DGRAM`); if that fails,
/// [`surge_ping::Client::new`] automatically falls back to a raw socket
/// (`SOCK_RAW`).  When neither is available the function logs a warning via
/// `eprintln!` and returns `reachable = false` with 100 % loss — it never
/// panics.
pub async fn icmp_ping(kind: TargetKind, addr: Ipv4Addr, timeout_dur: Duration) -> ProbeOutcome {
    use socket2::Type;
    use surge_ping::{Client, Config, ICMP};

    let config = Config::builder()
        .kind(ICMP::V4)
        .sock_type_hint(Type::DGRAM)
        .build();

    let client = match Client::new(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[linewatch] warning: cannot create ICMP socket for {}: {}",
                addr, e
            );
            return ProbeOutcome {
                kind,
                reachable: false,
                rtt: None,
                loss_pct: 100,
            };
        }
    };

    use rand::random;
    use surge_ping::{PingIdentifier, PingSequence};

    let ident = PingIdentifier(random());
    let mut pinger = client.pinger(std::net::IpAddr::V4(addr), ident).await;
    pinger.timeout(timeout_dur);

    let payload: &[u8] = b"linewatch";
    const COUNT: usize = 3;
    let mut successes: usize = 0;
    let mut total_rtt_ms: u128 = 0;

    for seq in 0..COUNT as u16 {
        match pinger.ping(PingSequence(seq), payload).await {
            Ok((_packet, rtt)) => {
                total_rtt_ms += rtt.as_millis();
                successes += 1;
            }
            Err(surge_ping::SurgeError::Timeout { .. }) => {
                // Timeout is expected for lost packets — no warning.
            }
            Err(e) => {
                eprintln!(
                    "[linewatch] warning: ICMP seq {} to {} failed: {}",
                    seq, addr, e
                );
            }
        }
    }

    let loss_pct = ((COUNT - successes) * 100 / COUNT) as u8;
    let reachable = successes > 0;
    let mean_rtt = if successes > 0 {
        Some(Duration::from_millis(
            (total_rtt_ms / successes as u128) as u64,
        ))
    } else {
        None
    };

    ProbeOutcome {
        kind,
        reachable,
        rtt: mean_rtt,
        loss_pct,
    }
}

// ---------------------------------------------------------------------------
// Tests (live, require network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::TargetKind;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn probe_kind(name: &str) -> TargetKind {
        match name {
            "tcp" => TargetKind::TcpAnchor,
            "http" => TargetKind::Http,
            "dns" => TargetKind::Dns,
            "icmp" => TargetKind::IcmpAnchor,
            _ => TargetKind::Gateway,
        }
    }

    #[tokio::test]
    async fn live_tcp_connect() {
        let o = tcp_connect(probe_kind("tcp"), "1.1.1.1:443", TIMEOUT).await;
        println!(
            "TCP 1.1.1.1:443  reachable={}  rtt={:?}",
            o.reachable, o.rtt
        );
        assert!(o.reachable, "TCP connect to 1.1.1.1:443 should succeed");
    }

    #[tokio::test]
    async fn live_http_check() {
        let o = http_check(
            probe_kind("http"),
            "https://www.google.com/generate_204",
            TIMEOUT,
        )
        .await;
        println!(
            "HTTP generate_204  reachable={}  rtt={:?}",
            o.reachable, o.rtt
        );
        // generate_204 returns 204 No Content
        assert!(o.reachable, "HTTP check should succeed");
    }

    #[tokio::test]
    async fn live_dns_check() {
        let o = dns_check(probe_kind("dns"), "1.1.1.1:53", "cloudflare.com", TIMEOUT).await;
        println!(
            "DNS cloudflare.com @1.1.1.1  reachable={}  rtt={:?}",
            o.reachable, o.rtt
        );
        assert!(o.reachable, "DNS lookup should resolve");
    }

    #[tokio::test]
    async fn live_icmp_ping() {
        // Ping the default gateway (or 1.1.1.1 as a reliable public target)
        let gw = default_gateway().await.unwrap_or(Ipv4Addr::new(1, 1, 1, 1));
        let o = icmp_ping(probe_kind("icmp"), gw, TIMEOUT).await;
        println!(
            "ICMP {}  reachable={}  rtt={:?}  loss={}%",
            gw, o.reachable, o.rtt, o.loss_pct
        );
        // Don't assert — ICMP may be firewalled; just print.
    }

    #[tokio::test]
    async fn live_default_gateway() {
        let gw = default_gateway().await;
        println!("Default gateway: {:?}", gw);
        // We can't assert that it's Some because the test environment might
        // not have a default route, but we print it.
    }
}
