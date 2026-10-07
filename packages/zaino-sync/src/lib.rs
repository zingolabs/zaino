//! Index sync plumbing: the final stream ([`IndexerDataSink<Final>`]) → each index's writer loop
//!
//! - One sender (`zaino-nfs`), every step final: each height once, ascending, never retracted
//! - Each writer: its own loop over its [`Subscription`], its store behind a [`Committer`]
//! - An index's per-block output for another = a second sink (value-balance → [`FeeSink`])

#![forbid(unsafe_code)]

mod committer;
mod data_sink;
mod emit;
mod final_block;
mod offload;
mod report;

pub use committer::{held, Committer, Run};
pub use data_sink::{Applied, IndexerDataSink, Step, Subscription, Weight};
pub use emit::describe_metrics;
pub use final_block::{Final, Folds};
pub use offload::compute;
use offload::Offloaded;
pub use report::Human;

use zaino_primitives::types::{Block, BlockFees};

impl Weight for Block {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

/// Per-block transaction fees: value-balance → compact-block, one per unfolded final step
pub type FeeSink = IndexerDataSink<BlockFees>;

impl Weight for BlockFees {
    fn weight(&self) -> usize {
        self.footprint()
    }
}
