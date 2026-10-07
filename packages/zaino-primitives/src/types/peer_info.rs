//! `getpeerinfo`: the backing validator's peer connections (Zaino has no p2p peers)

/// `addr` opaque (Tor / I2P / hostnames != socket addresses; forwarded, never inspected)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub addr: String,
    pub inbound: bool,
}
