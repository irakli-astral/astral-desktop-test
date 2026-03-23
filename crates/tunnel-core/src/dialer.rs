use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures_util::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{info, warn};

use crate::{TunnelEvent, TunnelStats};

/// Ports the tunnel is allowed to dial. Requests for other ports are rejected.
const ALLOWED_PORTS: &[u16] = &[80, 443, 8080, 8443];

/// Handle a single inbound yamux stream from the relay.
///
/// Protocol:
/// 1. Read first line: target address (e.g. "linkedin.com:443\n")
/// 2. Connect to that target from the local network
/// 3. Pipe bytes bidirectionally between yamux stream and TCP connection
/// 4. Update stats when done
pub async fn handle_stream(
    mut yamux_stream: yamux::Stream,
    stats: Arc<TunnelStats>,
    event_tx: tokio::sync::mpsc::Sender<TunnelEvent>,
) {
    // Read the target address (first line, terminated by \n)
    let target = match read_target(&mut yamux_stream).await {
        Some(t) => t,
        None => {
            warn!("Failed to read target from yamux stream");
            return;
        }
    };

    info!(target = %target, "Stream opened, dialing target");

    // Validate target: port whitelist + SSRF protection
    if let Err(e) = validate_target(&target) {
        warn!(target = %target, error = %e, "Target validation rejected");
        let _ = event_tx
            .send(TunnelEvent::StreamClosed {
                target: target.clone(),
            })
            .await;
        return;
    }

    stats.active_streams.fetch_add(1, Ordering::Relaxed);
    stats.total_streams.fetch_add(1, Ordering::Relaxed);
    let _ = event_tx
        .send(TunnelEvent::StreamOpened {
            target: target.clone(),
        })
        .await;

    // Connect to the target from the local network (residential IP!)
    let tcp_stream = match TcpStream::connect(&target).await {
        Ok(s) => s,
        Err(e) => {
            warn!(target = %target, error = %e, "Failed to connect to target");
            stats.active_streams.fetch_sub(1, Ordering::Relaxed);
            let _ = event_tx
                .send(TunnelEvent::StreamClosed {
                    target: target.clone(),
                })
                .await;
            return;
        }
    };

    // Bridge yamux (futures-io) → tokio-io via compat layer, then copy bidirectionally.
    // yamux::Stream implements futures::AsyncRead/AsyncWrite.
    // .compat() converts to tokio::io::AsyncRead/AsyncWrite.
    // TcpStream already implements tokio::io::AsyncRead/AsyncWrite.
    let mut yamux_compat = yamux_stream.compat();
    let mut tcp_stream = tcp_stream;

    match tokio::io::copy_bidirectional(&mut yamux_compat, &mut tcp_stream).await {
        Ok((up, down)) => {
            stats.bytes_up.fetch_add(up, Ordering::Relaxed);
            stats.bytes_down.fetch_add(down, Ordering::Relaxed);
            info!(target = %target, bytes_up = up, bytes_down = down, "Stream completed");
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::ConnectionReset {
                warn!(target = %target, error = %e, "Stream error");
            }
        }
    }

    stats.active_streams.fetch_sub(1, Ordering::Relaxed);
    let _ = event_tx.send(TunnelEvent::StreamClosed { target }).await;
}

/// Validate a dial target: must have an allowed port and must not resolve to a private IP.
fn validate_target(target: &str) -> Result<(), String> {
    let (_host, port) = parse_host_port(target)?;

    if !ALLOWED_PORTS.contains(&port) {
        return Err(format!(
            "Port {port} not in allowed list: {ALLOWED_PORTS:?}"
        ));
    }

    // Resolve DNS and check all resulting IPs
    let addrs = target
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {target}: {e}"))?;

    let mut count = 0;
    for addr in addrs {
        count += 1;
        if is_private_ip(&addr.ip()) {
            return Err(format!(
                "Target {target} resolves to private/loopback IP: {}",
                addr.ip()
            ));
        }
    }

    if count == 0 {
        return Err(format!("DNS resolution returned no addresses for {target}"));
    }

    Ok(())
}

/// Parse host and port from a target string. Supports `host:port` and `[ipv6]:port`.
fn parse_host_port(target: &str) -> Result<(&str, u16), String> {
    // Handle IPv6 bracket notation [::1]:443
    if let Some(bracket_end) = target.rfind("]:") {
        let port_str = &target[bracket_end + 2..];
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("Invalid port: {port_str}"))?;
        let host = &target[..=bracket_end];
        return Ok((host, port));
    }
    // Standard host:port
    let colon = target
        .rfind(':')
        .ok_or_else(|| format!("No port in target: {target}"))?;
    let port_str = &target[colon + 1..];
    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("Invalid port: {port_str}"))?;
    let host = &target[..colon];
    Ok((host, port))
}

/// Check if an IP address is private, loopback, or otherwise non-routable.
/// Handles IPv4-mapped IPv6 addresses (e.g. ::ffff:127.0.0.1) to prevent SSRF bypasses.
fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => {
            // Convert IPv4-mapped IPv6 (::ffff:x.x.x.x) to IPv4 and re-check
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_ipv4(&mapped);
            }
            v6.is_loopback() || v6.is_unspecified() || is_link_local_v6(v6) || v6.is_multicast()
        }
    }
}

fn is_private_ipv4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_cgnat(ip)
}

/// Check for CGNAT range 100.64.0.0/10
fn is_cgnat(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xC0) == 64
}

fn is_link_local_v6(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// Read the target address from the first line of the yamux stream.
/// The relay sends "host:port\n" as the first message.
async fn read_target(stream: &mut yamux::Stream) -> Option<String> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];

    // Read byte-by-byte until we hit '\n' or reach a reasonable limit
    for _ in 0..256 {
        match stream.read(&mut byte).await {
            Ok(0) => return None, // EOF before newline
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
            }
            Err(_) => return None,
        }
    }

    let target = String::from_utf8(buf).ok()?;
    let target = target.trim().to_string();

    if target.is_empty() {
        return None;
    }

    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_parse_host_port_standard() {
        let (host, port) = parse_host_port("example.com:443").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_parse_host_port_ipv6() {
        let (host, port) = parse_host_port("[::1]:443").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_parse_host_port_no_port() {
        assert!(parse_host_port("example.com").is_err());
    }

    #[test]
    fn test_parse_host_port_invalid_port() {
        assert!(parse_host_port("example.com:abc").is_err());
    }

    #[test]
    fn test_is_private_ip_loopback_v4() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[test]
    fn test_is_private_ip_loopback_v6() {
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn test_is_private_ip_private_ranges() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
    }

    #[test]
    fn test_is_private_ip_ipv4_mapped_v6() {
        // ::ffff:127.0.0.1 — IPv4-mapped IPv6 SSRF bypass
        let mapped = IpAddr::V6("::ffff:127.0.0.1".parse().unwrap());
        assert!(is_private_ip(&mapped));

        // ::ffff:10.0.0.1
        let mapped_private = IpAddr::V6("::ffff:10.0.0.1".parse().unwrap());
        assert!(is_private_ip(&mapped_private));
    }

    #[test]
    fn test_is_private_ip_public() {
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn test_is_private_ip_cgnat() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 255
        ))));
        // Outside CGNAT range
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
    }

    #[test]
    fn test_is_private_ip_link_local() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
    }

    #[test]
    fn test_is_private_ip_unspecified() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn test_validate_target_blocked_port() {
        assert!(validate_target("example.com:22").is_err());
        assert!(validate_target("example.com:3306").is_err());
    }

    #[test]
    fn test_validate_target_allowed_port_public() {
        // google.com:443 should pass — public IP, allowed port
        assert!(validate_target("google.com:443").is_ok());
    }

    #[test]
    fn test_validate_target_loopback_rejected() {
        assert!(validate_target("127.0.0.1:443").is_err());
        assert!(validate_target("localhost:443").is_err());
    }

    #[test]
    fn test_validate_target_private_rejected() {
        assert!(validate_target("192.168.1.1:443").is_err());
        assert!(validate_target("10.0.0.1:80").is_err());
    }
}
