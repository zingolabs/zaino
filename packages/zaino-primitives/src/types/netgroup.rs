//! Peers grouped by the operator plausibly behind them: one attacker = many addresses, few groups
//!
//! - submission: one entry per netgroup
//! - finality: a block's peer witnesses counted per netgroup (`zaino-header-chain`)

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

/// Peers in more netgroups than this = not one operator (eclipse needs addresses in that many /16s,
/// 6 of the default 8 outbound peers)
///
/// - required by peer finality & mempool quorum
pub const ECLIPSE_NETGROUPS: usize = 5;

/// Address prefix one operator plausibly controls: /16 IPv4, /32 IPv6 (Bitcoin Core's buckets)
///
/// - IPv4-mapped IPv6 = IPv4 (one host, two spellings)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Netgroup {
    V4([u8; 2]),
    V6([u8; 4]),
}

impl Netgroup {
    /// Distinct netgroups among `peers`
    pub fn count(peers: impl IntoIterator<Item = SocketAddr>) -> usize {
        peers.into_iter().map(Self::of).collect::<BTreeSet<_>>().len()
    }

    pub fn of(addr: SocketAddr) -> Self {
        let v4 = |[a, b, ..]: [u8; 4]| Self::V4([a, b]);
        match addr.ip() {
            IpAddr::V4(ip) => v4(ip.octets()),
            IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
                Some(mapped) => v4(mapped.octets()),
                None => {
                    let [a, b, c, d, ..] = ip.octets();
                    Self::V6([a, b, c, d])
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn ipv4_groups_by_16_ipv6_by_32_and_a_mapped_address_by_its_ipv4() {
        let v4 = SocketAddr::from((Ipv4Addr::new(10, 1, 9, 9), 1));
        let mapped = SocketAddr::from((Ipv4Addr::new(10, 1, 2, 3).to_ipv6_mapped(), 2));
        assert_eq!(Netgroup::of(v4), Netgroup::of(mapped), "mapped v6 = its v4 /16");
        let other_16 = SocketAddr::from((Ipv4Addr::new(10, 2, 9, 9), 1));
        assert_ne!(Netgroup::of(v4), Netgroup::of(other_16), "another /16");
        let v6 = |a: u16| SocketAddr::from((Ipv6Addr::new(0x2001, 0xdb8, a, 0, 0, 0, 0, 1), 3));
        assert_eq!(Netgroup::of(v6(1)), Netgroup::V6([0x20, 0x01, 0x0d, 0xb8]));
        assert_eq!(Netgroup::of(v6(1)), Netgroup::of(v6(2)), "/32: same operator block");
    }
}
