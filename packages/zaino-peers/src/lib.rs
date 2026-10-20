#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod metrics;
mod network;
mod wire;

#[cfg(test)]
mod tests;

pub use metrics::describe_metrics;
pub use network::{PeerConfig, PeerError, PeerNetwork, PushError, Supplied, DEFAULT_PEER_TARGET};
pub use wire::WireError;
