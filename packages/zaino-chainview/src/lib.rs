#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod config;
mod endpoints;
mod error;
mod fold;
mod headers;
mod holders;
mod observe;
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

pub use endpoints::{Agreement, EndpointSet, ValidatorMetadata};
pub use error::{ConfigError, HeaderStoreFailed, SubmitError};
pub use headers::HeaderSync;
pub use observe::ObservationFold;
pub use peers::PeerWatch;
pub use ports::{Heard, ValidatorP2pSource};
pub use snapshot::{ChainViewSnapshot, Count, MempoolEntry, MempoolView, Projection, Spread};
pub use submit::SubmitPolicy;
pub use telemetry::{describe_metrics, Alarms, METRIC_BUCKETS};
pub use view::{ChainView, ChainViewSubscriber};
