//! ChainView: one coherent read surface over the whole chain.
//!
//! A chain view answers questions about the chain by composing three providers
//! it does not own: the finalised store below the reorg seam
//! (`zaino-chain-store`), the bounded recent graph above it
//! (`zaino-chain-head`), and the validator. What it adds is coherence — a
//! pinned view answering as of one tip — and an honest account of where each
//! answer came from.
//!
//! # How a read is answered
//!
//! Each provider covers a range of heights:
//!
//! ```text
//! store   [genesis, watermark]     durable, a single chain at that depth
//! head    [floor, tip]             the recent window, with competing branches
//! source  everything               the validator
//! ```
//!
//! The store starts at genesis by contract, and the head's window is sized to
//! overlap it, so in steady state the two meet. While the store is still
//! building they do not, and the range between them is served by the validator.
//! **That hole is the normal state during sync, not a degraded corner** — a
//! read there is filled, never refused.
//!
//! Reading a range therefore means splitting it across whichever providers
//! cover it and stitching the answers in height order. A single-height read is
//! that with one segment. There is no second mechanism.
//!
//! # What degrades, and what refuses
//!
//! Almost everything can be filled from the validator. Two things cannot:
//!
//! - **Cumulative chainwork** is the sum from genesis, so knowing it needs an
//!   unbroken chain *below* the block, not the block itself. The store carries
//!   it directly, and a chain-head block is rebased onto it — the head's work
//!   is measured from an anchor the store eventually reaches, so the absolute
//!   value is one addition once it has. It is `None` for anything the validator
//!   answered, and for everything at or above the first hole — see
//!   [`ChainBlock::chainwork`].
//! - **Spend status and the txout set** come from Zaino's own indexes, which
//!   the validator does not run. A read that would span a hole is refused
//!   rather than answered incompletely.
//!
//! # What a chain view is not
//!
//! Chain-only. The mempool is not part of it and this crate does not depend on
//! `zaino-mempool`: a mempool is not a fact about the chain but about what
//! might join it. Transaction broadcast, node and network information
//! (`getinfo`, `getpeerinfo`, `getmininginfo`), RPC serving and daemon
//! lifecycle are likewise above this layer, and reach the validator through
//! `zaino-source` directly rather than through here.

pub mod block;
pub mod capability;
pub mod error;
pub mod ports;
pub mod source;
pub mod types;

pub use block::ChainBlock;
pub use capability::{
    Answerable, ChainCapability, ServedCapabilities, ServiceabilityManifest, ServiceableRange,
};
pub use error::{ChainViewError, Result};
pub use ports::{
    AddressRead, BlockRead, ChainView, ChainViewSnapshot, CompactBlockRead, ForkReconcile,
    SpendRead, TransactionRead, TreestateRead, TxOutSetRead,
};
pub use source::ChainViewSource;
pub use types::{
    BlockId, ChainScope, ChainTxPosition, Locator, RawTransaction, SpendStatus,
    TransactionLocations,
};

pub mod composer;

pub use composer::{
    ChainViewComposer, ChainViewComposerBuilder, ChainViewConfig, ChainViewSync, ComposerSnapshot,
};

#[cfg(feature = "testing")]
pub mod testing;
