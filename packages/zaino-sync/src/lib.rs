//! Index sync pipeline: [`Producer`] → [`BlockSink`] → subscribed indexes
//!
//! - One fetch, one decode: the [`Producer`] adds each [`Block`](zaino_primitives::types::Block)
//!   to the [`BlockSink`] (bulk, then the quorum tip through the chain head)
//! - Each index subscribes ([`IndexerDataSink::subscribe`]); its [`IndexFollower`] drives its
//!   [`IndexWriter`] from that [`Subscription`]
//! - An index's per-block output ([`Derives`]) = its follower's steps republished into another
//!   sink (e.g. [`FeeSink`]); a consumer reads it in lockstep beside its block
//!   subscription ([`Zip`])
//! - Invariant: every index sees contiguous ascending heights from after the rearmost durable
//!   tip (an index ahead skips in `deliver`; indexes assert, never tolerate)

#![forbid(unsafe_code)]

mod data_sink;
mod emit;
mod feed;
mod follower;
mod index_writer;
mod offload;
mod producer;
mod publisher;
mod report;
mod served;

pub use data_sink::{IndexerDataSink, Step, Subscription, Weight};
pub use emit::describe_metrics;
pub use feed::{DerivedFrom, Feed, Paired, Zip};
pub use follower::{Downstream, FollowError, IndexFollower};
pub use index_writer::{finalize_now, Derives, IndexWriter, Linked};
pub use offload::{blocking, compute, Offloaded};
pub use producer::{ProduceError, Producer};
pub use served::{Reads, Served};

use zaino_primitives::types::{Block, BlockFees};

/// The one decoded block stream every index subscribes to
pub type BlockSink = IndexerDataSink<Block>;

impl Weight for Block {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

/// Per-block transaction fees, published step for step by the value-balance index
pub type FeeSink = IndexerDataSink<BlockFees>;

/// A block and its fees, read in lockstep off [`BlockSink`] and [`FeeSink`]
pub type BlockWithFees = Paired<Block, BlockFees>;

impl Weight for BlockFees {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

impl DerivedFrom<Block> for BlockFees {
    fn derived_from(&self, block: &Block) -> bool {
        self.belongs_to(block)
    }
}
