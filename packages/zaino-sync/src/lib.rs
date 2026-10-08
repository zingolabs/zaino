//! The final path: [`FinalFollower`] → [`IndexerDataSink<Block>`] → each index's writer loop
//! (`docs/design/data-sink.md`)
//!
//! - One sender, every step final: each height once, ascending, never retracted
//! - Each writer: its own loop over its [`Subscription`], its store behind a [`Committer`]
//! - An index's per-block output for another = a second sink (value-balance → [`FeeSink`])

#![forbid(unsafe_code)]

mod committer;
mod data_sink;
mod emit;
mod fetch;
mod follower;
mod handle;
mod offload;
mod per_index;
mod progress;
mod report;

pub use committer::{held, Committer, Run};
pub use data_sink::{Applied, IndexerDataSink, Step, Subscription, Weight};
pub use emit::describe_metrics;
pub use fetch::{check_block, check_block_at, fetch, fetch_at, merkle_root, Checked, Misanswer};
pub use follower::{FinalFollower, FollowError};
pub use handle::IndexHandle;
pub use offload::compute;
use offload::Offloaded;
pub use per_index::PerIndex;
pub use progress::SyncProgress;
pub use report::{ByteSize, Human};

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
