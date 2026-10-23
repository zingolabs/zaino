//! `getpeerinfo`: the backing validator's peer connections (Zaino has no p2p peers)

/// `addr` opaque: forwarded, never inspected (Tor / I2P / hostnames != socket addresses)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub addr: String,
    pub inbound: bool,
}
