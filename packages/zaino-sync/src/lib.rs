//! The final path: [`FinalizedFollower`] → [`IndexerDataSink<Block>`] → each index's writer loop
//! (`docs/architecture.md` §7)
//!
//! - One sender, every step final: each height once, ascending, never retracted
//! - Each writer: its own loop over its [`Subscription`]'s runs, its store published through an
//!   [`IndexPublisher`]
//! - An index's per-block output for another = a second sink (value-balance → [`FeeSink`])

#![forbid(unsafe_code)]

mod data_sink;
mod emit;
mod fetch;
mod follower;
mod handle;
mod offload;
mod progress;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod writer;

pub use data_sink::{IndexerDataSink, Run, Step, Subscription, Weight};
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub(crate) use fetch::fetch_at;
pub use fetch::{check_block, fetch, Checked, Misanswer};
pub use follower::{FinalizedFollower, FollowError};
pub use handle::IndexHandle;
pub use offload::{blocking, compute};
pub use progress::SyncProgress;
pub use writer::{apply, assert_next, assert_run, commit, held, ran, IndexPublisher};

use zaino_primitives::types::{Block, BlockFees};

impl Weight for Block {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

/// Per-block transaction fees: value-balance → compact-block, one per final step
pub type FeeSink = IndexerDataSink<BlockFees>;

impl Weight for BlockFees {
    fn weight(&self) -> usize {
        self.footprint()
    }
}
