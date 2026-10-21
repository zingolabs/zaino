//! Egress queue delay: how long this node's own sends wait in a queue, from the listener's sockets
//! (`tcp_info` via sock_diag, as `ss`) → the overload latch (`overload.rs`)
//!
//! - per socket: smoothed RTT − its min RTT = queueing on the path (no link speed, NIC or hypervisor
//!   knowledge: sees qdisc, virtual NIC, hypervisor shaper alike)
//! - verdict only across >= `egress_min_networks` client networks (one heavy downloader or a proxy
//!   in front = one network: its own last mile, never this node's)
//! - retransmits = a policing link's symptom (drops instead of queueing), same breadth rule

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

/// Sample spacing (sockets that sent within it count: an idle socket's RTT is stale)
pub(crate) const SAMPLE_PERIOD: Duration = Duration::from_secs(5);

/// Segments a network must send in a sample for its retransmit share to count
const MIN_SEGMENTS: u64 = 100;

/// One socket's `tcp_info` fields the verdict reads
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TcpSample {
    pub(crate) last_data_sent_ms: u32,
    pub(crate) rtt_us: u32,
    pub(crate) total_retrans: u32,
    pub(crate) min_rtt_us: u32,
    pub(crate) data_segs_out: u32,
}

/// Kernel `struct tcp_info` (host byte order, append-only ABI) → [`TcpSample`]
///
/// - `None` = shorter than `data_segs_out`'s end (kernel < 4.10: no `min_rtt`)
pub(crate) fn decode_tcp_info(bytes: &[u8]) -> Option<TcpSample> {
    let u32_at = |at: usize| Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    Some(TcpSample {
        last_data_sent_ms: u32_at(44)?,
        rtt_us: u32_at(68)?,
        total_retrans: u32_at(100)?,
        min_rtt_us: u32_at(148)?,
        data_segs_out: u32_at(156)?,
    })
}

/// A client network: IPv4 /24 (v4-mapped IPv6 included), IPv6 /48
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Network {
    V4(u32),
    V6(u64),
}

pub(crate) fn network(ip: IpAddr) -> Network {
    match ip {
        IpAddr::V4(v4) => Network::V4(u32::from(v4) & 0xffff_ff00),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => Network::V4(u32::from(v4) & 0xffff_ff00),
            None => Network::V6((u128::from(v6) >> 80) as u64),
        },
    }
}

/// One established listener socket (`cookie` = the kernel's id for it, stable across samples)
#[derive(Clone, Copy, Debug)]
pub(crate) struct Connection {
    pub(crate) cookie: u64,
    pub(crate) network: Network,
    pub(crate) tcp: TcpSample,
}

/// One sample's verdict (`None` = fewer client networks sending than the breadth rule needs)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Sample {
    pub(crate) networks: usize,
    pub(crate) queue_delay: Option<Duration>,
    pub(crate) retransmit: Option<f64>,
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    match values.len() {
        0 => None,
        len if len % 2 == 1 => Some(values[mid]),
        _ => Some((values[mid - 1] + values[mid]) / 2.0),
    }
}

/// Retransmitted, sent segments per socket at the last sample (deltas: this sample's share)
pub(crate) type Counters = HashMap<u64, (u32, u32)>;

/// `connections` now → this sample's verdict + the counters the next one diffs against
///
/// - queue delay: median per network of its sending sockets, then median across networks
/// - retransmits: per network Σ retransmitted / Σ sent since `previous` (sockets new since then
///   count from zero), median across networks that sent >= MIN_SEGMENTS
pub(crate) fn sample(
    connections: &[Connection],
    previous: &Counters,
    min_networks: usize,
) -> (Sample, Counters) {
    let sent_within = SAMPLE_PERIOD.as_millis() as u32;
    let mut delays: HashMap<Network, Vec<f64>> = HashMap::new();
    let mut segments: HashMap<Network, (u64, u64)> = HashMap::new();
    let mut counters = Counters::with_capacity(connections.len());
    for connection in connections {
        let tcp = connection.tcp;
        counters.insert(connection.cookie, (tcp.total_retrans, tcp.data_segs_out));
        if tcp.last_data_sent_ms > sent_within {
            continue;
        }
        let queued = tcp.rtt_us.saturating_sub(tcp.min_rtt_us);
        delays.entry(connection.network).or_default().push(f64::from(queued));
        let (retrans, sent) = previous.get(&connection.cookie).copied().unwrap_or((0, 0));
        let network = segments.entry(connection.network).or_default();
        network.0 += u64::from(tcp.total_retrans.saturating_sub(retrans));
        network.1 += u64::from(tcp.data_segs_out.saturating_sub(sent));
    }

    let networks = delays.len();
    let broad = |per_network: Vec<f64>| {
        (per_network.len() >= min_networks).then(|| median(per_network)).flatten()
    };
    let queue_delay = broad(delays.into_values().filter_map(median).collect())
        .map(|us| Duration::from_micros(us as u64));
    let retransmit = broad(
        segments
            .into_values()
            .filter(|&(_, sent)| sent >= MIN_SEGMENTS)
            .map(|(retrans, sent)| retrans as f64 / sent as f64)
            .collect(),
    );
    (Sample { networks, queue_delay, retransmit }, counters)
}

/// Samples since the last serving summary
#[derive(Debug, Default)]
pub(crate) struct Window {
    samples: Mutex<Vec<Sample>>,
}

/// A window's verdict: medians of its samples' (only samples with breadth count)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WindowVerdict {
    pub(crate) networks: usize,
    pub(crate) queue_delay: Option<Duration>,
    pub(crate) retransmit: Option<f64>,
}

impl Window {
    pub(crate) fn push(&self, sample: Sample) {
        self.samples.lock().expect("egress window mutex poisoned").push(sample);
    }

    /// The window's verdict, the next window started empty
    pub(crate) fn take(&self) -> WindowVerdict {
        let samples =
            std::mem::take(&mut *self.samples.lock().expect("egress window mutex poisoned"));
        let networks = median(samples.iter().map(|s| s.networks as f64).collect()).unwrap_or(0.0);
        let delay =
            median(samples.iter().filter_map(|s| s.queue_delay).map(|d| d.as_secs_f64()).collect());
        WindowVerdict {
            networks: networks as usize,
            queue_delay: delay.map(Duration::from_secs_f64),
            retransmit: median(samples.iter().filter_map(|s| s.retransmit).collect()),
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::run;

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::sync::Arc;

    use netlink_packet_core::{
        NetlinkHeader, NetlinkMessage, NetlinkPayload, NLM_F_DUMP, NLM_F_REQUEST,
    };
    use netlink_packet_sock_diag::constants::{AF_INET, AF_INET6, IPPROTO_TCP};
    use netlink_packet_sock_diag::inet::nlas::Nla;
    use netlink_packet_sock_diag::inet::{
        ExtensionFlags, InetRequest, InetResponse, SocketId, StateFlags,
    };
    use netlink_packet_sock_diag::SockDiagMessage;
    use netlink_sys::{protocols::NETLINK_SOCK_DIAG, Socket, SocketAddr};
    use tokio_util::sync::CancellationToken;
    use tracing::warn;

    use super::{decode_tcp_info, network, sample, Connection, Counters, Window, SAMPLE_PERIOD};
    use crate::AdmissionState;

    /// Samples `port`'s established sockets every SAMPLE_PERIOD into `window` until `cancel`
    ///
    /// - breadth rule = `admission`'s live `egress_min_networks` (an operator may change it)
    /// - a failed dump (no sock_diag: container seccomp, non-netlink kernel) = WARN once, then the
    ///   egress causes stay without a verdict
    pub(crate) async fn run(
        port: u16,
        admission: Arc<AdmissionState>,
        window: Arc<Window>,
        cancel: CancellationToken,
    ) {
        let mut ticks = tokio::time::interval(SAMPLE_PERIOD);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous = Counters::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticks.tick() => {}
            }
            let dumped = tokio::task::spawn_blocking(move || dump(port)).await;
            let connections = match dumped {
                Ok(Ok(connections)) => connections,
                Ok(Err(error)) => {
                    warn!(%error, "Egress sampling unavailable (sock_diag); overload ignores egress");
                    return;
                }
                Err(error) => {
                    warn!(%error, "Egress sampling task failed; overload ignores egress");
                    return;
                }
            };
            let min_networks = admission.egress_min_networks();
            let (verdict, counters) = sample(&connections, &previous, min_networks);
            previous = counters;
            window.push(verdict);
        }
    }

    /// Every established TCP socket on local `port`, both families (dual-stack listeners hold
    /// IPv4 clients as v4-mapped IPv6)
    fn dump(port: u16) -> io::Result<Vec<Connection>> {
        let mut socket = Socket::new(NETLINK_SOCK_DIAG)?;
        socket.bind_auto()?;
        socket.connect(&SocketAddr::new(0, 0))?;
        let mut connections = Vec::new();
        for (family, socket_id) in [(AF_INET, SocketId::new_v4()), (AF_INET6, SocketId::new_v6())] {
            request(&socket, family, socket_id)?;
            receive(&socket, |response| {
                if response.header.socket_id.source_port != port {
                    return;
                }
                let tcp = response.nlas.iter().find_map(|nla| match nla {
                    Nla::TcpInfo(bytes) => decode_tcp_info(bytes),
                    _ => None,
                });
                if let Some(tcp) = tcp {
                    let id = &response.header.socket_id;
                    connections.push(Connection {
                        cookie: u64::from_ne_bytes(id.cookie),
                        network: network(id.destination_address),
                        tcp,
                    });
                }
            })?;
        }
        Ok(connections)
    }

    fn request(socket: &Socket, family: u8, socket_id: SocketId) -> io::Result<()> {
        let mut header = NetlinkHeader::default();
        header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        let mut packet = NetlinkMessage::new(
            header,
            SockDiagMessage::InetRequest(InetRequest {
                family,
                protocol: IPPROTO_TCP,
                extensions: ExtensionFlags::INFO,
                states: StateFlags::ESTABLISHED,
                socket_id,
            })
            .into(),
        );
        packet.finalize();
        let mut buf = vec![0; packet.buffer_len()];
        packet.serialize(&mut buf);
        socket.send(&buf, 0)?;
        Ok(())
    }

    /// Each response of one dump to `each`, until its Done
    fn receive(socket: &Socket, mut each: impl FnMut(&InetResponse)) -> io::Result<()> {
        let invalid = |error: String| io::Error::new(io::ErrorKind::InvalidData, error);
        let mut buf = vec![0; 1 << 16];
        loop {
            let size = socket.recv(&mut &mut buf[..], 0)?;
            let mut offset = 0;
            while offset < size {
                let message = NetlinkMessage::<SockDiagMessage>::deserialize(&buf[offset..size])
                    .map_err(|error| invalid(error.to_string()))?;
                match message.payload {
                    NetlinkPayload::Done(_) => return Ok(()),
                    NetlinkPayload::Error(error) => return Err(error.to_io()),
                    NetlinkPayload::InnerMessage(SockDiagMessage::InetResponse(response)) => {
                        each(&response);
                    }
                    _ => {}
                }
                let length = message.header.length as usize;
                if length == 0 {
                    return Err(invalid("zero-length netlink message".to_owned()));
                }
                offset += length;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::io::{Read as _, Write as _};
        use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};

        use super::*;

        /// The real kernel: a loopback pair after 256 KiB sent → only the accepted side (source port =
        /// the listener's), its client network, live `tcp_info`
        #[test]
        fn dump_finds_the_listeners_established_sockets_with_their_tcp_info() {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("bound").port();
            let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            let (mut accepted, _) = listener.accept().expect("accept");
            let sent = vec![7u8; 256 * 1024];
            accepted.write_all(&sent).expect("send");
            let mut received = vec![0u8; sent.len()];
            client.read_exact(&mut received).expect("receive");

            let found = dump(port).expect("sock_diag dump");
            assert_eq!(found.len(), 1, "the accepted side; the client's source port is its own");
            let only = found[0];
            assert_eq!(only.network, network(IpAddr::V4(Ipv4Addr::LOCALHOST)));
            assert!(only.tcp.data_segs_out > 0, "{:?}", only.tcp);
            assert!(only.tcp.last_data_sent_ms < 5_000, "{:?}", only.tcp);
            assert!(
                only.tcp.rtt_us >= only.tcp.min_rtt_us && only.tcp.min_rtt_us > 0,
                "{:?}",
                only.tcp
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    /// Kernel layout: each field at its `struct tcp_info` offset, host byte order; short = None
    #[test]
    fn tcp_info_decodes_by_kernel_offset_and_refuses_a_pre_min_rtt_struct() {
        let mut bytes = vec![0u8; 232];
        let mut put =
            |at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        put(44, 1_250);
        put(68, 31_000);
        put(100, 7);
        put(148, 24_000);
        put(156, 9_000);
        let expected = TcpSample {
            last_data_sent_ms: 1_250,
            rtt_us: 31_000,
            total_retrans: 7,
            min_rtt_us: 24_000,
            data_segs_out: 9_000,
        };
        assert_eq!(decode_tcp_info(&bytes), Some(expected), "kernel >= 4.10 (232 bytes on 6.x)");
        assert_eq!(decode_tcp_info(&bytes[..160]), Some(expected), "exactly through data_segs_out");
        assert_eq!(decode_tcp_info(&bytes[..159]), None, "pre-4.10: no min_rtt / data_segs_out");
    }

    /// /24 and /48, v4-mapped IPv6 = its IPv4 network (dual-stack listener)
    #[test]
    fn clients_group_by_slash_24_and_slash_48() {
        let v4 = |a, b, c, d| network(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
        assert_eq!(v4(203, 0, 113, 7), v4(203, 0, 113, 200));
        assert_ne!(v4(203, 0, 113, 7), v4(203, 0, 114, 7));
        let mapped = IpAddr::V6(Ipv4Addr::new(203, 0, 113, 9).to_ipv6_mapped());
        assert_eq!(network(mapped), v4(203, 0, 113, 1));
        let v6 = |segments: [u16; 8]| network(IpAddr::V6(Ipv6Addr::from(segments)));
        assert_eq!(
            v6([0x2001, 0xdb8, 0xaa, 1, 0, 0, 0, 1]),
            v6([0x2001, 0xdb8, 0xaa, 2, 0, 0, 0, 9])
        );
        assert_ne!(
            v6([0x2001, 0xdb8, 0xaa, 1, 0, 0, 0, 1]),
            v6([0x2001, 0xdb8, 0xab, 1, 0, 0, 0, 1])
        );
    }

    /// The breadth rule, idle sockets, per-network medians and retransmit deltas in one sample:
    /// - one heavy network (10 sockets queued 20 ms) among quiet ones counts once
    /// - a socket idle past SAMPLE_PERIOD is ignored however stale its RTT
    /// - below `min_networks` sending = no verdict, retransmits included
    #[test]
    fn a_sample_is_the_median_over_networks_and_needs_breadth() {
        let tcp = |queued_ms: u32, idle_ms: u32, retrans: u32, segs: u32| TcpSample {
            last_data_sent_ms: idle_ms,
            rtt_us: 40_000 + queued_ms * 1000,
            total_retrans: retrans,
            min_rtt_us: 40_000,
            data_segs_out: segs,
        };
        let client = |cookie: u64, net: u8, tcp| Connection {
            cookie,
            network: network(IpAddr::V4(Ipv4Addr::new(198, 51, net, 1))),
            tcp,
        };
        // networks 0..4: 1, 2, 3, 4 ms; network 9: ten sockets at 20 ms; network 8: idle at 500 ms
        let mut connections: Vec<_> =
            (0u8..4).map(|n| client(n.into(), n, tcp(u32::from(n) + 1, 10, 0, 1_000))).collect();
        connections.extend((0..10).map(|k| client(100 + k, 9, tcp(20, 10, 0, 1_000))));
        connections.push(client(200, 8, tcp(500, 60_000, 0, 1_000)));

        let (verdict, counters) = sample(&connections, &Counters::new(), 5);
        assert_eq!(verdict.networks, 5, "idle network 8 not counted");
        assert_eq!(verdict.queue_delay, Some(Duration::from_millis(3)), "median of 1,2,3,4,20 ms");
        assert_eq!(verdict.retransmit, Some(0.0));
        assert_eq!(counters.len(), connections.len(), "every socket's counters kept");

        // next sample: network 0 retransmitted 300 of 1,000 new segments, the rest none
        let next: Vec<_> = connections
            .iter()
            .map(|c| Connection {
                tcp: TcpSample { data_segs_out: c.tcp.data_segs_out + 1_000, ..c.tcp },
                ..*c
            })
            .map(|c| match c.cookie {
                0 => Connection { tcp: TcpSample { total_retrans: 300, ..c.tcp }, ..c },
                _ => c,
            })
            .collect();
        let (verdict, _) = sample(&next, &counters, 5);
        assert_eq!(verdict.retransmit, Some(0.0), "median network: none retransmitted");

        let (narrow, _) = sample(&connections, &Counters::new(), 6);
        assert_eq!(narrow, Sample { networks: 5, queue_delay: None, retransmit: None });
    }

    /// A window = medians of its samples' verdicts; samples without breadth carry no verdict
    #[test]
    fn a_window_is_the_median_of_its_samples_and_starts_empty() {
        let window = Window::default();
        let ms = Duration::from_millis;
        for (delay, retransmit) in [
            (Some(ms(2)), Some(0.01)),
            (Some(ms(14)), Some(0.0)),
            (Some(ms(18)), None),
            (None, None),
        ] {
            window.push(Sample { networks: 30, queue_delay: delay, retransmit });
        }
        let verdict = window.take();
        assert_eq!(verdict.networks, 30);
        assert_eq!(verdict.queue_delay, Some(ms(14)), "median of 2, 14, 18 ms");
        assert_eq!(verdict.retransmit, Some(0.005), "median of 1 % and 0 %");
        assert_eq!(window.take(), WindowVerdict::default(), "next window starts empty");
    }
}
