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
pub use publisher::{Publisher, SnapshotError, Snapshots};
pub use report::{describe_metrics, emit_gauges, indexes, Report};
pub use snapshot::{ForkView, Snapshot, Tips, Unavailable, Unready};
