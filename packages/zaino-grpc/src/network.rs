//! Network queueing on this listener's own sockets (`tcp_info` via sock_diag, as `ss -ti`) →
//! `zaino_grpc_queue_delay_seconds` + the overload latch's network verdict
//!
//! - per socket: smoothed RTT − min RTT = time its data waited in a queue (no link speed needed)
//! - one observation per client network (/24, /48) per SAMPLE: its sockets' median (a heavy client
//!   counts once); low quantiles rise only when a queue on this node delays every network

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::metrics;

const SAMPLE: Duration = Duration::from_secs(10);

/// A socket's client network (IPv4 /24, v4-mapped included; IPv6 /48) and its queueing delay
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Queued {
    pub(crate) network: IpAddr,
    pub(crate) delay_us: u32,
}

/// `tcp_info` (host order, append-only kernel ABI) of a socket to `peer` → `None` if it sent
/// nothing this SAMPLE (stale RTT) or predates `min_rtt` (kernel < 4.10)
pub(crate) fn decode_tcp_info(peer: IpAddr, bytes: &[u8]) -> Option<Queued> {
    let u32_at = |at: usize| Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let (last_data_sent_ms, rtt_us, min_rtt_us) = (u32_at(44)?, u32_at(68)?, u32_at(148)?);
    let network = match peer {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            IpAddr::V6((u128::from(v6) & !0u128 << 80).into())
        }
        IpAddr::V6(v6) => IpAddr::V4((u32::from(v6.to_ipv4_mapped()?) & 0xffff_ff00).into()),
        IpAddr::V4(v4) => IpAddr::V4((u32::from(v4) & 0xffff_ff00).into()),
    };
    let sending = u128::from(last_data_sent_ms) <= SAMPLE.as_millis();
    sending.then(|| Queued { network, delay_us: rtt_us.saturating_sub(min_rtt_us) })
}

/// Each client network's median delay
pub(crate) fn per_network(sockets: &[Queued]) -> Vec<Duration> {
    let mut by_network = HashMap::<IpAddr, Vec<u32>>::new();
    for socket in sockets {
        by_network.entry(socket.network).or_default().push(socket.delay_us);
    }
    let median = |mut delays: Vec<u32>| {
        delays.sort_unstable();
        Duration::from_micros(delays[delays.len() / 2].into())
    };
    by_network.into_values().map(median).collect()
}

/// Each SAMPLE's per-network delays since the last overload window
#[derive(Debug, Default)]
pub(crate) struct Window(Mutex<Vec<Vec<Duration>>>);

impl Window {
    /// The 20th percentile of the delays of samples with >= `min_networks` networks (`None` = no
    /// such sample), the next window started empty
    pub(crate) fn queue_delay(&self, min_networks: usize) -> Option<Duration> {
        let samples = std::mem::take(&mut *self.0.lock().expect("network window poisoned"));
        let mut delays: Vec<Duration> =
            samples.into_iter().filter(|sample| sample.len() >= min_networks).flatten().collect();
        delays.sort_unstable();
        delays.get(delays.len() / 5).copied()
    }
}

/// Samples `port` every SAMPLE into the histogram + `window` until `cancel` (sock_diag
/// unavailable = WARN once, then stops)
pub(crate) async fn run(port: u16, window: Arc<Window>, cancel: CancellationToken) {
    let mut ticks = tokio::time::interval(SAMPLE);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let delays = match tokio::task::spawn_blocking(move || sockets(port)).await {
            Ok(Ok(sockets)) => per_network(&sockets),
            failed => {
                warn!(?failed, "Network queueing unmeasured (sock_diag)");
                return;
            }
        };
        delays.iter().copied().for_each(metrics::queue_delay);
        window.0.lock().expect("network window poisoned").push(delays);
    }
}

/// Established sockets on local `port` (`Unsupported` off Linux)
#[cfg(not(target_os = "linux"))]
fn sockets(_port: u16) -> std::io::Result<Vec<Queued>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Established sockets on local `port`, both families (a dual-stack listener holds IPv4 clients as
/// v4-mapped IPv6)
#[cfg(target_os = "linux")]
fn sockets(port: u16) -> std::io::Result<Vec<Queued>> {
    use std::io;

    use netlink_packet_core::{NetlinkMessage, NetlinkPayload, NLM_F_DUMP, NLM_F_REQUEST};
    use netlink_packet_sock_diag::constants::{AF_INET, AF_INET6, IPPROTO_TCP};
    use netlink_packet_sock_diag::inet::nlas::Nla;
    use netlink_packet_sock_diag::inet::{ExtensionFlags, InetRequest, SocketId, StateFlags};
    use netlink_packet_sock_diag::SockDiagMessage;
    use netlink_sys::{protocols::NETLINK_SOCK_DIAG, Socket, SocketAddr};

    let invalid = |error: String| io::Error::new(io::ErrorKind::InvalidData, error);
    let mut socket = Socket::new(NETLINK_SOCK_DIAG)?;
    socket.bind_auto()?;
    socket.connect(&SocketAddr::new(0, 0))?;
    let mut queued = Vec::new();
    for (family, socket_id) in [(AF_INET, SocketId::new_v4()), (AF_INET6, SocketId::new_v6())] {
        let request = InetRequest {
            family,
            protocol: IPPROTO_TCP,
            extensions: ExtensionFlags::INFO,
            states: StateFlags::ESTABLISHED,
            socket_id,
        };
        let mut packet = NetlinkMessage::from(SockDiagMessage::InetRequest(request));
        packet.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        packet.finalize();
        let mut buf = vec![0; packet.buffer_len()];
        packet.serialize(&mut buf);
        socket.send(&buf, 0)?;

        let mut buf = vec![0; 1 << 16];
        'dump: loop {
            let size = socket.recv(&mut &mut buf[..], 0)?;
            let mut offset = 0;
            while offset < size {
                let message = NetlinkMessage::<SockDiagMessage>::deserialize(&buf[offset..size])
                    .map_err(|error| invalid(error.to_string()))?;
                match message.payload {
                    NetlinkPayload::Done(_) => break 'dump,
                    NetlinkPayload::Error(error) => return Err(error.to_io()),
                    NetlinkPayload::InnerMessage(SockDiagMessage::InetResponse(response))
                        if response.header.socket_id.source_port == port =>
                    {
                        let peer = response.header.socket_id.destination_address;
                        queued.extend(response.nlas.iter().find_map(|nla| match nla {
                            Nla::TcpInfo(bytes) => decode_tcp_info(peer, bytes),
                            _ => None,
                        }));
                    }
                    _ => {}
                }
                match message.header.length as usize {
                    0 => return Err(invalid("zero-length netlink message".to_owned())),
                    length => offset += length,
                }
            }
        }
    }
    Ok(queued)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    /// Kernel offsets (`last_data_sent` 44, `rtt` 68, `min_rtt` 148), v4-mapped → its /24, idle
    /// and pre-4.10 structs dropped; one median per network however many sockets
    #[test]
    fn sockets_become_one_queueing_delay_per_client_network() {
        let mut bytes = vec![0u8; 232];
        for (at, value) in [(44, 1_250u32), (68, 31_000), (148, 24_000)] {
            bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        }
        let mapped = IpAddr::V6(Ipv4Addr::new(203, 0, 113, 9).to_ipv6_mapped());
        let expected = Queued { network: Ipv4Addr::new(203, 0, 113, 0).into(), delay_us: 7_000 };
        assert_eq!(decode_tcp_info(mapped, &bytes), Some(expected));
        assert_eq!(decode_tcp_info(mapped, &bytes[..151]), None, "no min_rtt");
        let mut idle = bytes.clone();
        idle[44..48].copy_from_slice(&60_000u32.to_ne_bytes());
        assert_eq!(decode_tcp_info(mapped, &idle), None, "idle a minute");

        let at = |net: u8, ms: u32| Queued {
            network: Ipv4Addr::new(198, 51, net, 0).into(),
            delay_us: ms * 1000,
        };
        let mut sockets = vec![at(1, 2), at(2, 5)];
        sockets.extend([900, 1, 950].map(|ms| at(9, ms)));
        let mut delays = per_network(&sockets);
        delays.sort();
        assert_eq!(delays, [2, 5, 900].map(Duration::from_millis), "network 9 once, its median");
    }

    /// A window's verdict: the low end over samples with breadth (a narrow sample ignored)
    #[test]
    fn a_window_is_the_low_end_of_its_broad_samples() {
        let ms = |ms: &[u64]| ms.iter().copied().map(Duration::from_millis).collect();
        let window = Window::default();
        window.0.lock().expect("window").extend([
            ms(&[1, 2, 3, 4, 900]),
            ms(&[6, 7, 8, 9, 10]),
            ms(&[500]),
        ]);
        assert_eq!(window.queue_delay(5), Some(Duration::from_millis(3)), "p20 of ten, not 500");
        assert_eq!(window.queue_delay(5), None, "next window starts empty");
    }

    /// The real kernel: a loopback pair after 256 KiB → only the accepted side (by port)
    #[cfg(target_os = "linux")]
    #[test]
    fn sockets_finds_the_listeners_established_sockets() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("bound").port();
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let (mut accepted, _) = listener.accept().expect("accept");
        accepted.write_all(&[7; 256 * 1024]).expect("send");
        client.read_exact(&mut [0; 256 * 1024]).expect("receive");

        let found = sockets(port).expect("sock_diag");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].network, IpAddr::from(Ipv4Addr::new(127, 0, 0, 0)));
    }
}
