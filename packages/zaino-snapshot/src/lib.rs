#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod compose;
mod feed;
mod publisher;
mod report;
mod snapshot;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use feed::{Logged, MempoolTail};
pub use publisher::{GrpcSnapshotError, GrpcSnapshots, Publisher};
pub use report::{describe_metrics, emit_gauges, notes};
pub use snapshot::{Down, GrpcSnapshot, Lightd, Standing, Tips, Unavailable};
