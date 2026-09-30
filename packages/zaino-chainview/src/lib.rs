#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod chain;
mod config;
mod endpoint;
mod endpoints;
mod error;
mod fold;
mod ports;
mod quorum;
mod snapshot;
mod telemetry;
mod view;

#[cfg(test)]
mod tests;

pub use endpoint::EndpointPoller;
pub use endpoints::{Agreement, EndpointSet, EndpointState, Ewma, ValidatorMetadata};
pub use error::{BelowQuorum, BroadcastError, ConfigError, EndpointPollError};
pub use ports::EndpointSource;
pub use quorum::{Quorum, QuorumTip};
pub use snapshot::{ChainViewSnapshot, MempoolEntry, MempoolView};
pub use telemetry::describe_metrics;
pub use view::{ChainView, ChainViewSubscriber, Endpoint, MempoolTail};
