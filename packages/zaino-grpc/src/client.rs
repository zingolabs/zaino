//! Who a connection is for the per-client cap: the peer, or behind a trusted proxy the source its
//! PROXY header (v1 or v2) names
//!
//! - A trusted peer MUST send the header (mixed = anyone reachable through it could forge one)
//! - Read under [`HEADER_TIMEOUT`] inside the connection task (never stalls accept)

use std::net::IpAddr;
use std::time::Duration;

use ipnet::IpNet;
use ppp::{v1, v2, HeaderResult, PartialResult as _};
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;

/// A proxy sends its header with the connection (slower = not a proxy)
pub(crate) const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// v1 ≤ 107 bytes; v2 = 16 + addresses + the TLVs real proxies add (AWS, Azure: tens of bytes)
const HEADER_MAX: usize = 4096;

/// Peers whose connections carry a PROXY header naming the real client
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<IpNet>);

impl TrustedProxies {
    pub fn new(networks: Vec<IpNet>) -> Self {
        Self(networks)
    }

    pub(crate) fn trusts(&self, peer: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(&peer))
    }
}

/// Why a trusted peer's connection was dropped
#[derive(Debug, thiserror::Error)]
pub(crate) enum HeaderError {
    #[error("no PROXY header within {HEADER_TIMEOUT:?}")]
    Timeout,
    #[error("PROXY header over {HEADER_MAX} bytes")]
    TooLong,
    #[error("connection closed before the PROXY header ended")]
    Closed,
    #[error("malformed PROXY header: {0}")]
    Malformed(String),
    #[error("reading the PROXY header: {0}")]
    Io(#[from] std::io::Error),
}

/// The header's source address + the bytes read past it (the client's first HTTP/2 bytes)
///
/// - `None` = no client to name (`LOCAL` health check, `UNKNOWN`, a unix socket): the proxy
///   counts as the client
pub(crate) async fn read_header(
    socket: &mut TcpStream,
) -> Result<(Option<IpAddr>, Vec<u8>), HeaderError> {
    let mut buffer = Vec::with_capacity(256);
    loop {
        if socket.read_buf(&mut buffer).await? == 0 {
            return Err(HeaderError::Closed);
        }
        let parsed = HeaderResult::parse(&buffer);
        if parsed.is_incomplete() {
            if buffer.len() >= HEADER_MAX {
                return Err(HeaderError::TooLong);
            }
            continue;
        }
        let (source, length) = source_of(parsed)?;
        return Ok((source, buffer.split_off(length)));
    }
}

/// Complete parse → (source, header length)
fn source_of(parsed: HeaderResult<'_>) -> Result<(Option<IpAddr>, usize), HeaderError> {
    match parsed {
        HeaderResult::V1(Ok(header)) => {
            let source = match header.addresses {
                v1::Addresses::Tcp4(ip) => Some(IpAddr::V4(ip.source_address)),
                v1::Addresses::Tcp6(ip) => Some(IpAddr::V6(ip.source_address)),
                v1::Addresses::Unknown => None,
            };
            Ok((source, header.header.len()))
        }
        HeaderResult::V2(Ok(header)) => {
            let source = match (header.command, header.addresses) {
                (v2::Command::Local, _) => None,
                (v2::Command::Proxy, v2::Addresses::IPv4(ip)) => {
                    Some(IpAddr::V4(ip.source_address))
                }
                (v2::Command::Proxy, v2::Addresses::IPv6(ip)) => {
                    Some(IpAddr::V6(ip.source_address))
                }
                (v2::Command::Proxy, v2::Addresses::Unspecified | v2::Addresses::Unix(_)) => None,
            };
            Ok((source, header.len()))
        }
        HeaderResult::V1(Err(error)) => Err(HeaderError::Malformed(error.to_string())),
        HeaderResult::V2(Err(error)) => Err(HeaderError::Malformed(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each header shape names its source and leaves the client's bytes after it untouched,
    /// however the header arrived split; garbage and an oversized header are refused
    #[tokio::test]
    async fn a_proxy_header_names_the_client_and_hands_back_the_rest_of_the_stream() {
        use tokio::io::AsyncWriteExt as _;

        let v2_ipv6 = v2::Builder::with_addresses(
            v2::Version::Two | v2::Command::Proxy,
            v2::Protocol::Stream,
            (
                "[2001:db8::7]:5000".parse::<std::net::SocketAddr>().expect("addr"),
                "[2001:db8::1]:443".parse::<std::net::SocketAddr>().expect("addr"),
            ),
        )
        .build()
        .expect("v2 header");
        let v2_local = v2::Builder::new(
            v2::Version::Two | v2::Command::Local,
            v2::AddressFamily::Unspecified | v2::Protocol::Unspecified,
        )
        .build()
        .expect("v2 local");
        let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        // v2 signature, PROXY over TCP4, then a declared 60,000-byte body that keeps coming
        let v2_huge =
            [b"\r\n\r\n\0\r\nQUIT\n\x21\x11\xea\x60".as_slice(), &[0u8; HEADER_MAX]].concat();
        let v1_long = [b"PROXY TCP4 ".as_slice(), &[b'1'; 200]].concat();

        // (case, header, writes it arrives in, source named or error prefix)
        type Case = (&'static str, Vec<u8>, usize, Result<Option<IpAddr>, &'static str>);
        let cases: [Case; 7] = [
            (
                "v1 tcp4",
                b"PROXY TCP4 203.0.113.9 10.0.0.1 51000 443\r\n".to_vec(),
                1,
                Ok(Some([203, 0, 113, 9].into())),
            ),
            ("v1 unknown", b"PROXY UNKNOWN\r\n".to_vec(), 3, Ok(None)),
            (
                "v2 ipv6, byte by byte",
                v2_ipv6,
                usize::MAX,
                Ok(Some("2001:db8::7".parse().expect("ip"))),
            ),
            ("v2 local health check", v2_local, 1, Ok(None)),
            ("not a header", b"GET / HTTP/1.1\r\n\r\n".to_vec(), 1, Err("malformed PROXY header")),
            ("v1 past its 107 bytes", v1_long, 1, Err("malformed PROXY header")),
            ("v2 past our cap", v2_huge, 1, Err("PROXY header over 4096 bytes")),
        ];

        for (case, header, chunks, expected) in cases {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("addr");
            let sent = [header.as_slice(), &preface].concat();
            let writer = tokio::spawn(async move {
                let mut client = TcpStream::connect(address).await.expect("connect");
                let per_write = header.len().div_ceil(chunks.min(header.len()));
                for piece in sent.chunks(per_write.max(1)) {
                    // A peer that refused us may have closed: later writes may fail
                    if client.write_all(piece).await.is_err() {
                        break;
                    }
                    client.flush().await.expect("flush");
                    tokio::task::yield_now().await;
                }
                client
            });
            let (mut socket, _) = listener.accept().await.expect("accept");

            let read = read_header(&mut socket).await;
            match expected {
                Ok(source) => {
                    let (got, mut rest) = read.unwrap_or_else(|e| panic!("{case}: {e}"));
                    assert_eq!(got, source, "{case}");
                    let _client = writer.await.expect("writer");
                    while rest.len() < preface.len() {
                        socket.read_buf(&mut rest).await.expect("rest of the stream");
                    }
                    assert_eq!(rest, preface, "{case}: the client's bytes, whole and in order");
                }
                Err(message) => {
                    let error = read.expect_err(case).to_string();
                    assert!(error.starts_with(message), "{case}: {error}");
                }
            }
        }

        let trusted = TrustedProxies::new(vec!["10.0.0.0/8".parse().expect("net")]);
        let peers: [IpAddr; 2] = [[10, 1, 2, 3].into(), [192, 168, 1, 1].into()];
        assert_eq!(peers.map(|peer| trusted.trusts(peer)), [true, false]);
    }
}
