//! Index sync pipeline: [`Producer`] → [`BlockSink`] → each index's own loop
//!
//! - One fetch, one decode: the [`Producer`] adds each [`Block`] to the [`BlockSink`] (bulk, then
//!   the quorum tip through the chain head)
//! - Each index subscribes ([`IndexerDataSink::subscribe`]), spawns its own loop over its
//!   [`Subscription`] and publishes through a [`Published`]
//! - An index's per-block output = one step it sends into another sink per step it follows
//!   (value-balance → [`FeeSink`]); a consumer awaits one off each queue per step
//! - Chain identity checked once, in the [`Producer`]: every block links onto the one before it,
//!   and onto every index's durable tip (indexes trust the stream)
//! - Invariant: every index sees contiguous ascending heights from after the rearmost durable tip
//!   (an index ahead skips heights it holds; indexes assert, never tolerate)

#![forbid(unsafe_code)]

mod data_sink;
mod emit;
mod offload;
mod producer;
mod published;
mod publisher;
mod report;
mod served;

pub use data_sink::{IndexerDataSink, Step, Subscription, Weight};
pub use emit::describe_metrics;
pub use offload::{blocking, compute, Offloaded};
pub use producer::{ProduceError, Producer};
pub use published::Published;
pub use report::Human;
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

impl Weight for BlockFees {
    fn weight(&self) -> usize {
        self.footprint()
    }
}
