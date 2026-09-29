//! `getpeerinfo`: the backing validator's peer connections (Zaino has no p2p peers)

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Opaque (Tor / I2P / hostnames are not socket addresses; forwarded, never inspected)
    pub addr: String,
    pub inbound: bool,
}
