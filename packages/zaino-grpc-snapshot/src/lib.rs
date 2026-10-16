#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod compose;
mod feed;
mod grpc_snapshot;
mod publisher;
mod report;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use feed::{Logged, MempoolTail};
pub use grpc_snapshot::{Down, GrpcSnapshot, Lightd, Standing, Tips, Unavailable};
pub use publisher::{GrpcSnapshotError, GrpcSnapshots, Publisher};
pub use report::{emit_gauges, notes, METRICS};
