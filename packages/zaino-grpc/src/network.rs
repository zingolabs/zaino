//! Network saturation on this listener's own sockets (`tcp_info` via sock_diag, as `ss -ti`) →
//! `zaino_grpc_queue_delay_seconds`, `zaino_grpc_retransmit_ratio` + the overload latch's verdict
//!
//! - one observation per client network (/24, /48) per SAMPLE (a heavy client counts once): low
//!   quantiles rise only when this node delays / drops for every network
//! - queueing = smoothed RTT − the network's base RTT: min RTT over BASE_MINUTES, every socket
//!   (RFC 6817 §3.4.2 base delay: a socket born into a queue never sees the unqueued RTT)
//! - loss = retransmitted / sent data segments since the last SAMPLE (a policer drops, no queue)

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::metrics;

const SAMPLE: Duration = Duration::from_secs(10);
const MINUTE_SAMPLES: u32 = (60 / SAMPLE.as_secs()) as u32;
/// Base RTT memory, one minimum per minute (RFC 6817 BASE_HISTORY: a path change ages out)
const BASE_MINUTES: usize = 10;
/// Data segments a network must send in a SAMPLE for its loss to count (2 % = 2 of them)
const MIN_SEGMENTS: u64 = 100;

/// One established socket's `tcp_info` (`min_rtt_us` `None` = no RTT sample yet)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TcpSocket {
    pub(crate) cookie: u64,
    pub(crate) network: IpAddr,
    pub(crate) sending: bool,
    pub(crate) rtt_us: u32,
    pub(crate) min_rtt_us: Option<u32>,
    pub(crate) retransmitted: u32,
    pub(crate) data_segments: u32,
}

/// Client network: IPv4 /24 (v4-mapped included), IPv6 /48
fn network(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => network(v4.into()),
            None => IpAddr::V6((u128::from(v6) & !0u128 << 80).into()),
        },
        IpAddr::V4(v4) => IpAddr::V4((u32::from(v4) & 0xffff_ff00).into()),
    }
}

/// `tcp_info` (host order, append-only kernel ABI) of socket `cookie` to `peer` → `None` if it
/// predates `data_segs_out` (kernel < 4.10)
pub(crate) fn decode_tcp_info(cookie: u64, peer: IpAddr, bytes: &[u8]) -> Option<TcpSocket> {
    let u32_at = |at: usize| Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let min_rtt_us = u32_at(148)?;
    Some(TcpSocket {
        cookie,
        network: network(peer),
        sending: u128::from(u32_at(44)?) <= SAMPLE.as_millis(),
        rtt_us: u32_at(68)?,
        // kernel ~0U = never measured
        min_rtt_us: (min_rtt_us != u32::MAX).then_some(min_rtt_us),
        retransmitted: u32_at(100)?,
        data_segments: u32_at(156)?,
    })
}

/// One SAMPLE: each sending network's median queueing + each network's loss (>= MIN_SEGMENTS)
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Sample {
    pub(crate) delays: Vec<Duration>,
    pub(crate) losses: Vec<f64>,
}

/// Memory across samples (`bases` minute `u32::MAX` = unobserved, `counters` = per cookie
/// (retransmitted, data segments) at the last sample)
#[derive(Debug, Default)]
pub(crate) struct History {
    bases: HashMap<IpAddr, [u32; BASE_MINUTES]>,
    minute: usize,
    minute_samples: u32,
    counters: HashMap<u64, (u32, u32)>,
}

impl History {
    /// `sockets` now → this sample (a socket's loss counts from its second sample: before it =
    /// SYN-ACK retries, or before zainod)
    pub(crate) fn sample(&mut self, sockets: &[TcpSocket]) -> Sample {
        if self.minute_samples == MINUTE_SAMPLES {
            let minute = (self.minute + 1) % BASE_MINUTES;
            self.bases.retain(|_, minutes| {
                minutes[minute] = u32::MAX;
                minutes.iter().any(|&min| min != u32::MAX)
            });
            (self.minute, self.minute_samples) = (minute, 0);
        }
        self.minute_samples += 1;

        for socket in sockets {
            let Some(min_rtt) = socket.min_rtt_us else { continue };
            let minutes = self.bases.entry(socket.network).or_insert([u32::MAX; BASE_MINUTES]);
            minutes[self.minute] = minutes[self.minute].min(min_rtt);
        }
        let mut delays = HashMap::<IpAddr, Vec<u32>>::new();
        for socket in sockets.iter().filter(|socket| socket.sending) {
            if let Some(&base) = self.bases.get(&socket.network).and_then(|m| m.iter().min()) {
                let delay = socket.rtt_us.saturating_sub(base);
                delays.entry(socket.network).or_default().push(delay);
            }
        }

        let mut segments = HashMap::<IpAddr, (u64, u64)>::new();
        let mut counters = HashMap::with_capacity(sockets.len());
        for socket in sockets {
            let now = (socket.retransmitted, socket.data_segments);
            if let Some(&(retransmitted, sent)) = self.counters.get(&socket.cookie) {
                let network = segments.entry(socket.network).or_default();
                network.0 += u64::from(now.0.wrapping_sub(retransmitted));
                network.1 += u64::from(now.1.wrapping_sub(sent));
            }
            counters.insert(socket.cookie, now);
        }
        self.counters = counters;

        let median = |mut delays: Vec<u32>| {
            delays.sort_unstable();
            Duration::from_micros(delays[delays.len() / 2].into())
        };
        Sample {
            delays: delays.into_values().map(median).collect(),
            losses: segments
                .into_values()
                .filter(|&(_, sent)| sent >= MIN_SEGMENTS)
                .map(|(retransmitted, sent)| retransmitted as f64 / sent as f64)
                .collect(),
        }
    }
}

/// One overload window's network signals (`None` = no sample with breadth)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) delay: Option<Duration>,
    pub(crate) loss: Option<f64>,
}

/// Each SAMPLE since the last overload window
#[derive(Debug, Default)]
pub(crate) struct Window(Mutex<Vec<Sample>>);

impl Window {
    /// 20th percentiles over the samples with >= `min_networks` networks (per signal), the next
    /// window started empty
    pub(crate) fn verdict(&self, min_networks: usize) -> Verdict {
        let samples = std::mem::take(&mut *self.0.lock().expect("network window poisoned"));
        let mut delays: Vec<Duration> = (samples.iter())
            .filter(|sample| sample.delays.len() >= min_networks)
            .flat_map(|sample| sample.delays.iter().copied())
            .collect();
        let mut losses: Vec<f64> = (samples.iter())
            .filter(|sample| sample.losses.len() >= min_networks)
            .flat_map(|sample| sample.losses.iter().copied())
            .collect();
        delays.sort_unstable();
        losses.sort_unstable_by(f64::total_cmp);
        Verdict {
            delay: delays.get(delays.len() / 5).copied(),
            loss: losses.get(losses.len() / 5).copied(),
        }
    }
}

/// Samples `port` every SAMPLE into the histograms + `window` until `cancel` (sock_diag
/// unavailable = WARN once, then stops)
pub(crate) async fn run(port: u16, window: Arc<Window>, cancel: CancellationToken) {
    let mut ticks = tokio::time::interval(SAMPLE);
    let mut history = History::default();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let sample = match tokio::task::spawn_blocking(move || sockets(port)).await {
            Ok(Ok(sockets)) => history.sample(&sockets),
            failed => {
                warn!(?failed, "Network saturation unmeasured (sock_diag)");
                return;
            }
        };
        sample.delays.iter().copied().for_each(metrics::queue_delay);
        sample.losses.iter().copied().for_each(metrics::retransmit_ratio);
        window.0.lock().expect("network window poisoned").push(sample);
    }
}

/// Established sockets on local `port` (`Unsupported` off Linux)
#[cfg(not(target_os = "linux"))]
fn sockets(_port: u16) -> std::io::Result<Vec<TcpSocket>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Established sockets on local `port`, both families (a dual-stack listener holds IPv4 clients as
/// v4-mapped IPv6)
#[cfg(target_os = "linux")]
fn sockets(port: u16) -> std::io::Result<Vec<TcpSocket>> {
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
    let mut found = Vec::new();
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
                        let id = &response.header.socket_id;
                        let cookie = u64::from_ne_bytes(id.cookie);
                        found.extend(response.nlas.iter().find_map(|nla| match nla {
                            Nla::TcpInfo(bytes) => {
                                decode_tcp_info(cookie, id.destination_address, bytes)
                            }
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
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    /// Kernel offsets (`last_data_sent` 44, `rtt` 68, `total_retrans` 100, `min_rtt` 148,
    /// `data_segs_out` 156), client networks (/24, v4-mapped = its /24, /48), unmeasured min RTT
    #[test]
    fn tcp_info_decodes_by_kernel_offset_into_its_client_network() {
        let mut bytes = vec![0u8; 232];
        for (at, value) in [(44, 1_250u32), (68, 31_000), (100, 7), (148, 24_000), (156, 9_000)] {
            bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        }
        let mapped = IpAddr::V6(Ipv4Addr::new(203, 0, 113, 9).to_ipv6_mapped());
        let expected = TcpSocket {
            cookie: 42,
            network: Ipv4Addr::new(203, 0, 113, 0).into(),
            sending: true,
            rtt_us: 31_000,
            min_rtt_us: Some(24_000),
            retransmitted: 7,
            data_segments: 9_000,
        };
        assert_eq!(decode_tcp_info(42, mapped, &bytes), Some(expected));
        assert_eq!(decode_tcp_info(42, mapped, &bytes[..159]), None, "pre-4.10");

        let mut idle = bytes.clone();
        idle[44..48].copy_from_slice(&60_000u32.to_ne_bytes());
        idle[148..152].copy_from_slice(&u32::MAX.to_ne_bytes());
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0xaa, 1, 0, 0, 0, 9));
        assert_eq!(
            decode_tcp_info(42, v6, &idle),
            Some(TcpSocket {
                network: Ipv6Addr::new(0x2001, 0xdb8, 0xaa, 0, 0, 0, 0, 0).into(),
                sending: false,
                min_rtt_us: None,
                ..expected
            }),
            "idle a minute, never measured, its /48"
        );
    }

    /// One network's sockets across samples:
    /// - sockets born into a queue (own min RTT 60 ms) measured against the network's 20 ms base,
    ///   the median socket once per network
    /// - base forgotten after BASE_MINUTES without a lower sample, a network with its sockets
    /// - loss: deltas from a socket's second sample, closed sockets dropped, < MIN_SEGMENTS ignored
    #[test]
    fn history_keeps_each_networks_base_rtt_and_loss_since_the_last_sample() {
        let socket =
            |cookie: u64, net: u8, rtt_ms: u32, min_rtt_ms: u32, retx: u32, segs: u32| TcpSocket {
                cookie,
                network: Ipv4Addr::new(198, 51, net, 0).into(),
                sending: true,
                rtt_us: rtt_ms * 1000,
                min_rtt_us: Some(min_rtt_ms * 1000),
                retransmitted: retx,
                data_segments: segs,
            };
        let ms = |ms: &[u64]| ms.iter().copied().map(Duration::from_millis).collect::<Vec<_>>();
        let sorted = |mut delays: Vec<Duration>| {
            delays.sort();
            delays
        };
        let mut history = History::default();

        let calm = history.sample(&[socket(1, 1, 21, 20, 0, 1_000), socket(9, 9, 5, 5, 0, 50)]);
        assert_eq!(sorted(calm.delays), ms(&[0, 1]));
        assert!(calm.losses.is_empty(), "first sight: no loss yet");

        let latecomers = [61, 62, 900].map(|rtt| socket(rtt.into(), 1, rtt, 60, 0, 0));
        let mut queued = vec![socket(1, 1, 61, 20, 30, 1_000 + 1_000), socket(9, 9, 5, 5, 3, 99)];
        queued.extend(latecomers);
        let sample = history.sample(&queued);
        assert_eq!(sorted(sample.delays), ms(&[0, 42]), "network 1: median of 41, 41, 42, 880");
        assert_eq!(sample.losses, [0.03], "network 1: 30 of 1,000; network 9: 49 sent");

        let later: Vec<_> = (0..MINUTE_SAMPLES as usize * BASE_MINUTES)
            .map(|_| history.sample(&latecomers))
            .collect();
        let last = later.last().expect("samples");
        assert_eq!(last.delays, ms(&[2]), "base aged out to the latecomers' 60 ms");
        assert!(last.losses.is_empty(), "under MIN_SEGMENTS");
        assert_eq!(history.bases.len(), 1, "network 9 forgotten");
        assert_eq!(history.counters.len(), 3, "closed sockets 1 + 9 dropped");
    }

    /// A window's verdict per signal: the low end over samples with breadth (a narrow sample
    /// ignored), next window empty
    #[test]
    fn a_window_is_the_low_end_of_its_broad_samples() {
        let ms = |ms: &[u64]| ms.iter().copied().map(Duration::from_millis).collect();
        let window = Window::default();
        window.0.lock().expect("window").extend([
            Sample { delays: ms(&[1, 2, 3, 4, 900]), losses: vec![0.0, 0.0, 0.2, 0.3, 0.4] },
            Sample { delays: ms(&[6, 7, 8, 9, 10]), losses: vec![0.5] },
            Sample { delays: ms(&[500]), losses: vec![0.1; 5] },
        ]);
        let verdict = Verdict { delay: Some(Duration::from_millis(3)), loss: Some(0.1) };
        assert_eq!(window.verdict(5), verdict, "p20 of ten each, not 500 ms / 50 %");
        assert_eq!(window.verdict(5), Verdict::default(), "next window starts empty");
    }

    /// The real kernel: a loopback pair after 256 KiB → only the accepted side (by port), measured
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
        let only = found[0];
        assert_eq!(only.network, IpAddr::from(Ipv4Addr::new(127, 0, 0, 0)));
        assert!(only.cookie != 0 && only.sending && only.data_segments > 0, "{only:?}");
        assert!(only.min_rtt_us.is_some_and(|min| min <= only.rtt_us), "{only:?}");
    }
}
