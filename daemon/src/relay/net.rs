use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use rand::RngCore;
use socket2::{Domain, Protocol as SockProto, Socket, Type};
use str0m::Candidate;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::config::Config;

const UDP_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub fn bind_shared_udp(port: u16) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProto::UDP))?;
    sock.set_nonblocking(true)?;
    if let Err(e) = sock.set_recv_buffer_size(UDP_BUFFER_BYTES) {
        debug!("Could not set UDP SO_RCVBUF to 16MB: {e}");
    }
    if let Err(e) = sock.set_send_buffer_size(UDP_BUFFER_BYTES) {
        debug!("Could not set UDP SO_SNDBUF to 16MB: {e}");
    }
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    sock.bind(&addr.into())?;
    let std_udp: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_udp)
}

pub async fn gather_local_candidates(
    socket: &UdpSocket,
    local_port: u16,
    config: &Config,
) -> (Vec<Candidate>, SocketAddr) {
    let mut candidates = Vec::new();

    // Always add loopback host candidate for local OBS WHIP.
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port);
    if let Ok(c) = Candidate::host(loopback, "udp") {
        candidates.push(c);
    }

    // Discover primary LAN IP via UDP route lookup (no packet sent).
    let lan_ip = discover_lan_ip();
    if let Some(ip) = lan_ip
        && !ip.is_loopback()
    {
        let lan_addr = SocketAddr::new(ip, local_port);
        if let Ok(c) = Candidate::host(lan_addr, "udp") {
            candidates.push(c);
        }
    }

    let base_addr = SocketAddr::new(lan_ip.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)), local_port);

    // Public / server-reflexive candidate: static public_ip if configured, else one-shot STUN.
    if let Some(pub_ip) = config.public_ip {
        let pub_addr = SocketAddr::new(pub_ip, local_port);
        if let Ok(c) = Candidate::server_reflexive(pub_addr, base_addr, "udp") {
            info!(public = %pub_addr, "Using configured static public IP candidate");
            candidates.push(c);
        }
    } else if !config.stun_server.is_empty() {
        match stun_discover_mapped_addr(socket, &config.stun_server).await {
            Ok(mapped) => {
                info!(srflx = %mapped, stun = %config.stun_server, "Discovered STUN candidate");
                if let Ok(c) = Candidate::server_reflexive(mapped, base_addr, "udp") {
                    candidates.push(c);
                }
            }
            Err(e) => {
                warn!(stun = %config.stun_server, "STUN discovery failed: {e} (continuing with host candidates only)");
            }
        }
    }

    (candidates, base_addr)
}

fn discover_lan_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip())
}

/// Minimal RFC 5389 STUN Binding Request on the shared UDP socket so the NAT mapping
/// matches the exact port used for WebRTC media.
async fn stun_discover_mapped_addr(
    socket: &UdpSocket,
    stun_server: &str,
) -> Result<SocketAddr, String> {
    let stun_addr = tokio::net::lookup_host(stun_server)
        .await
        .map_err(|e| e.to_string())?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| "no IPv4 address for STUN server".to_string())?;

    let mut tx_id = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tx_id);

    // 20-byte STUN Binding Request: type 0x0001, length 0, magic cookie 0x2112A442, tx_id
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    req[2..4].copy_from_slice(&0u16.to_be_bytes());
    req[4..8].copy_from_slice(&0x2112A442u32.to_be_bytes());
    req[8..20].copy_from_slice(&tx_id);

    socket
        .send_to(&req, stun_addr)
        .await
        .map_err(|e| e.to_string())?;

    let mut buf = [0u8; 512];
    let deadline = Instant::now() + Duration::from_secs(2);

    while Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(rem, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                if let Some(mapped) = parse_stun_binding_response(&buf[..n], &tx_id) {
                    return Ok(mapped);
                }
            }
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => break,
        }
    }

    Err("timed out waiting for STUN response".to_string())
}

fn parse_stun_binding_response(buf: &[u8], expected_tx_id: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if msg_type != 0x0101 || cookie != 0x2112A442 || &buf[8..20] != expected_tx_id {
        return None;
    }
    let end = (20 + msg_len).min(buf.len());
    let mut i = 20;
    while i + 4 <= end {
        let attr_type = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let attr_len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        let val_start = i + 4;
        let val_end = val_start + attr_len;
        if val_end > end {
            break;
        }
        // XOR-MAPPED-ADDRESS (0x0020) or MAPPED-ADDRESS (0x0001), IPv4 (family 0x01)
        if (attr_type == 0x0020 || attr_type == 0x0001) && attr_len >= 8 && buf[val_start + 1] == 1
        {
            let raw_port = u16::from_be_bytes([buf[val_start + 2], buf[val_start + 3]]);
            let mut ip = [
                buf[val_start + 4],
                buf[val_start + 5],
                buf[val_start + 6],
                buf[val_start + 7],
            ];
            let port = if attr_type == 0x0020 {
                let cookie_bytes = 0x2112A442u32.to_be_bytes();
                for (b, c) in ip.iter_mut().zip(cookie_bytes) {
                    *b ^= c;
                }
                raw_port ^ 0x2112
            } else {
                raw_port
            };
            return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
        }
        // STUN attributes are padded to 4-byte boundaries
        let padded = (attr_len + 3) & !3;
        i = val_start + padded;
    }
    None
}
