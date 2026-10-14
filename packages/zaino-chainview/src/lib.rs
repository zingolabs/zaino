#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod admission;
mod config;
mod endpoints;
mod error;
mod fold;
mod headers;
mod listings;
#[cfg(test)]
mod mempool_model;
mod netgroup;
mod observe;
mod peer_mempool;
mod peers;
mod ports;
mod snapshot;
mod submit;
mod telemetry;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod view;

#[cfg(test)]
mod network_model;
#[cfg(test)]
mod tests;

pub use endpoints::{Agreement, TrustedValidatorMeta, TrustedValidatorSet};
pub use error::{ConfigError, SubmitError};
pub use headers::HeaderSync;
pub use observe::ObservationFold;
pub use peer_mempool::PeerMempool;
pub use peers::PeerWatch;
pub use ports::{Heard, PeerSource, UnspentOutputs};
pub use snapshot::{ChainViewSnapshot, Count, MempoolEntry, MempoolView, Projection, Spread};
pub use submit::SubmitPolicy;
pub use telemetry::{describe_metrics, Alarms, METRIC_BUCKETS};
pub use view::{ChainView, ChainViewSubscriber};
