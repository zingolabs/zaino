//! Index sync pipeline: [`Producer`] → [`BlockSink`] → subscribed indexes
//!
//! - One fetch, one decode: the [`Producer`] adds each [`Block`](zaino_primitives::types::Block)
//!   to the [`BlockSink`] (bulk, then the quorum tip through the chain head)
//! - Each index subscribes ([`IndexerDataSink::subscribe`]); its [`IndexFollower`] drives its
//!   [`IndexWriter`] from that [`Subscription`]
//! - Derived sink = an index publishing per-block data from [`IndexWriter::deliver`] (e.g.
//!   [`ValueBalanceSink`]); a consumer pairs it with the block by height + hash
//! - Invariant: every index sees contiguous ascending heights from the rearmost durable extent
//!   (an index ahead skips in `deliver`; indexes assert, never tolerate)

#![forbid(unsafe_code)]

mod data_sink;
mod emit;
mod follower;
mod index_writer;
mod offload;
mod producer;
mod report;
mod served;

pub use data_sink::{IndexerDataSink, SinkBuilder, SinkGone, Step, Subscription, Weight};
#[cfg(feature = "prometheus")]
pub use emit::describe_metrics;
pub use follower::{FollowError, IndexFollower};
pub use index_writer::{finalize_now, IndexWriter, Linked};
pub use offload::{blocking, compute, Offloaded};
pub use producer::{ProduceError, Producer};
pub use served::Served;

/// The one decoded block stream every index subscribes to
pub type BlockSink = IndexerDataSink<zaino_primitives::types::Block>;

pub type BlockSinkBuilder = SinkBuilder<zaino_primitives::types::Block>;

impl Weight for zaino_primitives::types::Block {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

/// Per-block transaction value balances, published by the value-balance index
pub type ValueBalanceSink = IndexerDataSink<zaino_primitives::types::BlockValueBalances>;

pub type ValueBalanceSinkBuilder = SinkBuilder<zaino_primitives::types::BlockValueBalances>;

impl Weight for zaino_primitives::types::BlockValueBalances {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

impl Subscription<zaino_primitives::types::BlockValueBalances> {
    /// `block`'s balances: the next item naming it; `None` = the publisher stopped
    ///
    /// - Skipped: `Finalized`/`Reset` (the consumer's lifecycle follows its block stream) and
    ///   items a reorg left queued (another branch's hash)
    pub async fn balances_for(
        &mut self,
        block: &zaino_primitives::types::Block,
    ) -> Option<std::sync::Arc<zaino_primitives::types::BlockValueBalances>> {
        while let Some(step) = self.next().await {
            if let Step::Apply { data, .. } = step {
                if data.belongs_to(block) {
                    return Some(data);
                }
            }
        }
        None
    }
}
