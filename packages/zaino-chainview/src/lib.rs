#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod admission;
mod alarm_thresholds;
mod chain_view_core;
mod chain_view_snapshot;
mod error;
mod header_sync;
mod listings;
#[cfg(test)]
mod mempool_model_tests;
mod observation_merge;
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
mod mock_peers;
#[cfg(test)]
mod network_model_tests;
#[cfg(test)]
mod tests;

pub use alarm_thresholds::STALE_TIP_BLOCKS;
pub use chain_view_snapshot::{
    ChainViewSnapshot, MempoolEntry, MempoolState, MempoolView, Projection,
};
pub use error::SubmitError;
pub use header_sync::{HeaderSync, OwnChain};
pub use observation_merge::ObservationMerge;
pub use peer_mempool::PeerMempool;
pub use peers::PeerWatch;
pub use ports::{PeerSource, UnspentOutputs};
pub use submit::{SubmitPolicy, ValidatedTransaction};
pub use telemetry::{Alarm, Alarms, METRICS};
pub use trusted_validators::{Agreement, TrustedValidatorMeta, TrustedValidatorSet};
pub use view::{ChainView, ChainViewSubscriber};
