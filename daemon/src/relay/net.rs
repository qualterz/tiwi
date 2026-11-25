use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::RngCore;
use socket2::{Domain, Protocol as SockProto, Socket, Type};
use str0m::Candidate;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::config::Config;

// Asymmetric UDP buffers: 4 MB receive avoids multi-second bufferbloat under overload;
// 16 MB send absorbs high-fanout egress bursts across worker shards.
const UDP_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const UDP_SEND_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub fn bind_shared_udp(port: u16) -> std::io::Result<(UdpSocket, Arc<std::net::UdpSocket>)> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProto::UDP))?;
    sock.set_nonblocking(true)?;
    #[cfg(windows)]
    disable_udp_connreset(&sock);
    if let Err(e) = sock.set_recv_buffer_size(UDP_RECV_BUFFER_BYTES) {
        debug!("Could not set UDP SO_RCVBUF to 4MB: {e}");
    }
    if let Err(e) = sock.set_send_buffer_size(UDP_SEND_BUFFER_BYTES) {
        debug!("Could not set UDP SO_SNDBUF to 16MB: {e}");
    }
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    sock.bind(&addr.into())?;
    let std_udp: std::net::UdpSocket = sock.into();
    let tx_udp = std_udp.try_clone()?;
    tx_udp.set_nonblocking(true)?;
    Ok((UdpSocket::from_std(std_udp)?, Arc::new(tx_udp)))
}

#[cfg(windows)]
fn disable_udp_connreset(sock: &Socket) {
    use std::os::windows::io::AsRawSocket;

    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAIoctl(
            s: usize,
            dw_io_control_code: u32,
            lpv_in_buffer: *const u32,
            cb_in_buffer: u32,
            lpv_out_buffer: *mut u8,
            cb_out_buffer: u32,
            lpcb_bytes_returned: *mut u32,
            lp_overlapped: *mut u8,
            lp_completion_routine: *mut u8,
        ) -> i32;
    }

    let flag: u32 = 0; // FALSE
    let mut bytes_returned: u32 = 0;
    let res = unsafe {
        WSAIoctl(
            sock.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            &raw const flag,
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &raw mut bytes_returned,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if res != 0 {
        debug!(
            err = %std::io::Error::last_os_error(),
            "Could not disable SIO_UDP_CONNRESET"
        );
    }
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
