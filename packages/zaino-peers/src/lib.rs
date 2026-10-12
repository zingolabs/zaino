#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod network;
mod wire;

#[cfg(test)]
mod tests;

pub use network::{
    Announced, PeerConfig, PeerError, PeerNetwork, PushError, Supplied, DEFAULT_PEER_TARGET,
};
pub use wire::WireError;
