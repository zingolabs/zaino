#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod compose;
mod grpc_snapshot;
mod mempool_feed;
mod publisher;
mod report;

#[cfg(test)]
mod invariants;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;

pub use grpc_snapshot::{GrpcSnapshot, Lightd, ServingStatus, Tips, Unavailable};
pub use mempool_feed::{Logged, MempoolTail};
pub use publisher::{GrpcSnapshotError, GrpcSnapshots, Publisher};
pub use report::{describe_metrics, emit_gauges, notes};
