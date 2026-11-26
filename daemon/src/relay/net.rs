use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{Duration, Instant};

use rand::RngCore;
use socket2::{Domain, Protocol as SockProto, Socket, Type};
use str0m::Candidate;
use tokio::net::UdpSocket;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::config::Config;

// Ingest receives OBS media (high RX, tiny RTCP/STUN TX).
// 16 MB matches Bun's buffer sizing to avoid Windows UDP overflows during keyframe bursts.
const INGEST_RECV_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const INGEST_SEND_BUFFER_BYTES: usize = 4 * 1024 * 1024;

// Each egress shard handles viewer media TX (high TX) and viewer STUN/DTLS/RTCP RX (low RX).
const SHARD_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const SHARD_SEND_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub struct BoundCandidates {
    pub local_port: u16,
    pub loopback_addr: SocketAddr,
    pub host_addr: SocketAddr,
    pub v6_host_addr: Option<SocketAddr>,
    pub is_dual_stack: bool,
    pub stun_target: Option<SocketAddr>,
    pub last_stun_refresh: Option<Instant>,
    pub rtx_enabled: bool,
    pub candidates: Vec<Candidate>,
}

pub fn bind_ingest_udp(port: u16) -> std::io::Result<(UdpSocket, bool)> {
    let (std_udp, is_dual_stack) =
        bind_udp_socket(port, INGEST_RECV_BUFFER_BYTES, INGEST_SEND_BUFFER_BYTES)?;
    Ok((UdpSocket::from_std(std_udp)?, is_dual_stack))
}

pub fn bind_shard_udp(port: u16) -> std::io::Result<(UdpSocket, bool)> {
    let (std_udp, is_dual_stack) =
        bind_udp_socket(port, SHARD_RECV_BUFFER_BYTES, SHARD_SEND_BUFFER_BYTES)?;
    Ok((UdpSocket::from_std(std_udp)?, is_dual_stack))
}

/// Converts an IPv4 destination to an IPv4-mapped IPv6 address when sending on a dual-stack socket.
#[inline]
pub fn to_socket_addr(addr: SocketAddr, is_dual_stack: bool) -> SocketAddr {
    if is_dual_stack && let SocketAddr::V4(v4) = addr {
        SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
    } else {
        addr
    }
}

/// Unwraps IPv4-mapped IPv6 source addresses (`::ffff:a.b.c.d`) back to plain IPv4.
#[inline]
pub fn from_socket_addr(addr: SocketAddr) -> SocketAddr {
    if let SocketAddr::V6(v6) = addr
        && let Some(v4) = v6.ip().to_ipv4_mapped()
    {
        SocketAddr::V4(SocketAddrV4::new(v4, v6.port()))
    } else {
        addr
    }
}

fn bind_udp_socket(
    port: u16,
    recv_buf: usize,
    send_buf: usize,
) -> std::io::Result<(std::net::UdpSocket, bool)> {
    let (sock, is_dual_stack) = match try_bind_dual_stack(port) {
        Ok(s) => (s, true),
        Err(e) => {
            debug!(port, err = %e, "Dual-stack IPv6 UDP bind failed; falling back to IPv4");
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProto::UDP))?;
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
            s.bind(&addr.into())?;
            (s, false)
        }
    };
    sock.set_nonblocking(true)?;
    #[cfg(windows)]
    disable_udp_connreset(&sock);
    if let Err(e) = sock.set_recv_buffer_size(recv_buf) {
        debug!(bytes = recv_buf, err = %e, "Could not set UDP SO_RCVBUF");
    }
    if let Err(e) = sock.set_send_buffer_size(send_buf) {
        debug!(bytes = send_buf, err = %e, "Could not set UDP SO_SNDBUF");
    }
    Ok((sock.into(), is_dual_stack))
}

fn try_bind_dual_stack(port: u16) -> std::io::Result<Socket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(SockProto::UDP))?;
    sock.set_only_v6(false)?;
    let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port);
    sock.bind(&addr.into())?;
    Ok(sock)
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

/// Gathers host + server-reflexive ICE candidates for a batch of UDP sockets in parallel
/// (single LAN/IPv6 route lookup, single STUN DNS resolution, concurrent STUN probes).
pub async fn gather_all_candidates(
    sockets: Vec<(UdpSocket, bool)>,
    config: &Config,
) -> Vec<(UdpSocket, BoundCandidates)> {
    let lan_ip = discover_lan_ip();
    let v6_ip = discover_ipv6_ip();
    let stun_addr = if config.public_ip.is_none() && !config.stun_server.is_empty() {
        match resolve_stun_addr(&config.stun_server).await {
            Ok(addr) => Some(addr),
            Err(e) => {
                warn!(
                    stun = %config.stun_server,
                    err = %e,
                    "STUN DNS lookup failed; continuing with host candidates only"
                );
                None
            }
        }
    } else {
        None
    };

    let public_ip = config.public_ip;
    let rtx_enabled = true;
    let stun_label = config.stun_server.clone();
    let count = sockets.len();

    let mut set = JoinSet::new();
    for (idx, (socket, is_dual_stack)) in sockets.into_iter().enumerate() {
        let stun_label = stun_label.clone();
        set.spawn(async move {
            let local_port = socket.local_addr().expect("bound UDP socket").port();
            let loopback_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port);
            let host_addr = SocketAddr::new(
                lan_ip.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                local_port,
            );
            let v6_host_addr = if is_dual_stack {
                v6_ip.map(|ip| SocketAddr::new(ip, local_port))
            } else {
                None
            };

            let mut candidates = Vec::with_capacity(4);
            if let Ok(c) = Candidate::host(loopback_addr, "udp") {
                candidates.push(c);
            }
            if let Some(ip) = lan_ip
                && !ip.is_loopback()
                && let Ok(c) = Candidate::host(SocketAddr::new(ip, local_port), "udp")
            {
                candidates.push(c);
            }
            if let Some(v6_addr) = v6_host_addr
                && let Ok(c) = Candidate::host(v6_addr, "udp")
            {
                candidates.push(c);
            }

            let mut stun_target = None;
            let mut last_stun_refresh = None;

            if let Some(pub_ip) = public_ip {
                let pub_addr = SocketAddr::new(pub_ip, local_port);
                if let Ok(c) = Candidate::server_reflexive(pub_addr, host_addr, "udp") {
                    if idx == 0 {
                        info!(public = %pub_addr, "Using configured static public IP candidate");
                    }
                    candidates.push(c);
                }
            } else if let Some(stun_addr) = stun_addr {
                let target = to_socket_addr(stun_addr, is_dual_stack);
                stun_target = Some(target);
                match stun_discover_mapped_addr(&socket, target).await {
                    Ok(mapped) => {
                        last_stun_refresh = Some(Instant::now());
                        if idx == 0 {
                            info!(srflx = %mapped, stun = %stun_label, "Discovered STUN candidate");
                        } else {
                            debug!(shard = idx - 1, srflx = %mapped, "Discovered shard STUN candidate");
                        }
                        if let Ok(c) = Candidate::server_reflexive(mapped, host_addr, "udp") {
                            candidates.push(c);
                        }
                    }
                    Err(e) => {
                        if idx == 0 {
                            warn!(
                                stun = %stun_label,
                                err = %e,
                                "STUN discovery failed; continuing with host candidates only"
                            );
                        } else {
                            debug!(shard = idx - 1, err = %e, "Shard STUN discovery failed");
                        }
                    }
                }
            }

            (
                idx,
                socket,
                BoundCandidates {
                    local_port,
                    loopback_addr,
                    host_addr,
                    v6_host_addr,
                    is_dual_stack,
                    stun_target,
                    last_stun_refresh,
                    rtx_enabled,
                    candidates,
                },
            )
        });
    }

    let mut ordered: Vec<Option<(UdpSocket, BoundCandidates)>> = (0..count).map(|_| None).collect();
    while let Some(res) = set.join_next().await {
        if let Ok((idx, socket, info)) = res {
            ordered[idx] = Some((socket, info));
        }
    }

    ordered
        .into_iter()
        .map(|slot| slot.expect("candidate gathering task panicked"))
        .collect()
}

fn discover_lan_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip())
}

fn discover_ipv6_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("[::]:0").ok()?;
    sock.connect("[2001:4860:4860::8888]:80").ok()?;
    let IpAddr::V6(v6) = sock.local_addr().ok()?.ip() else {
        return None;
    };
    // Skip loopback, link-local (fe80::/10), and ULA (fc00::/7) addresses.
    if v6.is_loopback() || v6.is_unicast_link_local() || (v6.segments()[0] & 0xfe00) == 0xfc00 {
        return None;
    }
    Some(IpAddr::V6(v6))
}

async fn resolve_stun_addr(stun_server: &str) -> Result<SocketAddr, String> {
    tokio::net::lookup_host(stun_server)
        .await
        .map_err(|e| e.to_string())?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| "no IPv4 address for STUN server".to_string())
}

/// Sends a non-blocking RFC 5389 STUN Binding Request on a synchronous shard UDP socket.
pub fn send_stun_binding_request(
    socket: &std::net::UdpSocket,
    stun_target: SocketAddr,
) -> Option<[u8; 12]> {
    let mut tx_id = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tx_id);
    let req = build_stun_binding_request(&tx_id);
    socket.send_to(&req, stun_target).ok()?;
    Some(tx_id)
}

/// Synchronously refreshes the shard's `srflx` candidate when the shard is idle and its
/// NAT mapping may have expired, bounded by `timeout`.
pub fn refresh_srflx_on_idle(
    socket: &std::net::UdpSocket,
    bound: &mut BoundCandidates,
    timeout: Duration,
) {
    let Some(stun_target) = bound.stun_target else {
        return;
    };
    let Some(tx_id) = send_stun_binding_request(socket, stun_target) else {
        return;
    };
    bound.last_stun_refresh = Some(Instant::now());

    let mut buf = [0u8; 512];
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Some(mapped) = parse_stun_binding_response(&buf[..n], &tx_id) {
                    update_srflx_candidate(bound, mapped);
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

/// Replaces or appends the `srflx` candidate on `bound` with `mapped`.
pub fn update_srflx_candidate(bound: &mut BoundCandidates, mapped: SocketAddr) {
    bound.last_stun_refresh = Some(Instant::now());
    let Ok(new_cand) = Candidate::server_reflexive(mapped, bound.host_addr, "udp") else {
        return;
    };
    if let Some(existing) = bound
        .candidates
        .iter_mut()
        .find(|c| c.kind() == str0m::CandidateKind::ServerReflexive)
    {
        if existing.addr() != mapped {
            debug!(old = %existing.addr(), new = %mapped, "Updated shard STUN srflx candidate");
        }
        *existing = new_cand;
    } else {
        debug!(srflx = %mapped, "Discovered shard STUN srflx candidate");
        bound.candidates.push(new_cand);
    }
}

fn build_stun_binding_request(tx_id: &[u8; 12]) -> [u8; 20] {
    // 20-byte STUN Binding Request: type 0x0001, length 0, magic cookie 0x2112A442, tx_id
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    req[2..4].copy_from_slice(&0u16.to_be_bytes());
    req[4..8].copy_from_slice(&0x2112A442u32.to_be_bytes());
    req[8..20].copy_from_slice(tx_id);
    req
}

/// Minimal RFC 5389 STUN Binding Request on a bound UDP socket so the NAT mapping
/// matches the exact port used for WebRTC media.
async fn stun_discover_mapped_addr(
    socket: &UdpSocket,
    stun_addr: SocketAddr,
) -> Result<SocketAddr, String> {
    let mut tx_id = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tx_id);
    let req = build_stun_binding_request(&tx_id);

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

pub fn parse_stun_binding_response(buf: &[u8], expected_tx_id: &[u8; 12]) -> Option<SocketAddr> {
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
