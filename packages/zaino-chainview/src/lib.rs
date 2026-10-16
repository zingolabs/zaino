#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod admission;
mod chain_view_snapshot;
mod config;
mod error;
mod headers;
mod listings;
#[cfg(test)]
mod mempool_model;
mod merge;
mod netgroup;
mod observe;
mod peer_mempool;
mod peers;
mod ports;
mod submit;
mod telemetry;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod trusted_validators;
mod view;

#[cfg(test)]
mod network_model;
#[cfg(test)]
mod tests;

pub use chain_view_snapshot::{
    ChainViewSnapshot, MempoolEntry, MempoolState, MempoolView, Projection,
};
pub use config::STALE_TIP_BLOCKS;
pub use error::SubmitError;
pub use headers::HeaderSync;
pub use observe::ObservationMerge;
pub use peer_mempool::PeerMempool;
pub use peers::PeerWatch;
pub use ports::{PeerSource, UnspentOutputs};
pub use submit::SubmitPolicy;
pub use telemetry::{Alarms, METRICS};
pub use trusted_validators::{Agreement, TrustedValidatorMeta, TrustedValidatorSet};
pub use view::{ChainView, ChainViewSubscriber};
