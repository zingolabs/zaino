//! What a store can serve, how far it can serve it, and what schema it is on.

use core::fmt;

use zaino_primitives::types::BlockRef;

/// One thing a chain store may be able to answer.
///
/// Coarser than a method and finer than a trait bound: it names an *index*, on
/// the grounds that indexes are what a deployment chooses to build and what a
/// migration adds. A capability being absent is a fact about this store, not
/// about the chain.
///
/// # Interim
///
/// This is the storage-shaped view — one variant per index the finalised state
/// maintains — surfaced so `ChainIndex` keeps working while the subsystem is
/// extracted. It is **not** the capability vocabulary consumers should be
/// written against: that is domain-shaped ("address history", "spend status")
/// and answers *to what height*, and it arrives with ChainView. Treat this as
/// wiring with a known end date, and do not build a serving surface on it.
///
/// Named `StoreCapability` rather than `Capability` for the same reason: the
/// domain-level enum that supersedes it will want the shorter name, and two
/// types called `Capability` in one binary is a recurring confusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum StoreCapability {
    /// Heights, hashes, and the watermark. Always present: a store that
    /// cannot answer these is not a store.
    Core,
    /// Indexed blocks, as the store's own projection of them.
    StoredBlocks,
    /// Compact blocks, for wallet sync.
    CompactBlocks,
    /// Where a transaction was mined, and what is at a position.
    Transactions,
    /// Which transaction spent an outpoint, and what an outpoint held.
    SpentOutputs,
    /// The UTXO-set commitment and its counters.
    TxOutSet,
    /// Transparent address history within the finalised range.
    TransparentHistory,
}

impl fmt::Display for StoreCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            StoreCapability::Core => "core reads",
            StoreCapability::StoredBlocks => "stored blocks",
            StoreCapability::CompactBlocks => "compact blocks",
            StoreCapability::Transactions => "the transaction index",
            StoreCapability::SpentOutputs => "the spent-output index",
            StoreCapability::TxOutSet => "the txout-set accumulator",
            StoreCapability::TransparentHistory => "transparent address history",
        };
        f.write_str(name)
    }
}

/// The capabilities a store currently offers.
///
/// Runtime state, not a type-level fact. A store on an older schema genuinely
/// lacks indexes a newer one has, and gains them part-way through a migration,
/// so this can change over the life of one handle. A consumer that cached it
/// at open will be wrong later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreCapabilities(Vec<StoreCapability>);

impl StoreCapabilities {
    /// The set offering exactly these capabilities.
    pub fn new(mut capabilities: Vec<StoreCapability>) -> Self {
        capabilities.sort_unstable();
        capabilities.dedup();
        Self(capabilities)
    }

    /// Whether this store currently offers `capability`.
    pub fn contains(&self, capability: StoreCapability) -> bool {
        self.0.binary_search(&capability).is_ok()
    }

    /// Every capability offered, ascending.
    pub fn iter(&self) -> impl Iterator<Item = StoreCapability> + '_ {
        self.0.iter().copied()
    }
}

/// A schema version triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaVersion {
    /// Incompatible layout change. A store will not open a major it does not
    /// know.
    pub major: u32,
    /// Additive change — new indexes or new rows, readable by an older reader
    /// that ignores them.
    pub minor: u32,
    /// A change with no layout consequence.
    pub patch: u32,
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Where a store is in its migration lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MigrationState {
    /// On its target schema, with nothing outstanding.
    #[default]
    Settled,
    /// Moving from one schema to another. Some capabilities may be absent
    /// until it finishes.
    InProgress {
        /// The schema being migrated away from.
        from: SchemaVersion,
        /// The schema being migrated to.
        to: SchemaVersion,
    },
}

/// The schema a store is on, and whether it is moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSchema {
    /// The version currently on disk.
    pub version: SchemaVersion,
    /// Whether a migration is under way.
    pub migration: MigrationState,
}

/// Where a watermark's answer came from.
///
/// A store that is far behind can still answer reads by passing them to the
/// validator, which is how a freshly-created or long-stopped deployment stays
/// useful while it builds. That is a materially different answer from one
/// served out of the store's own committed history, and a consumer reasoning
/// about coherence needs to be able to tell them apart — so this is on the
/// watermark rather than hidden behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Answers come from the store's own committed data.
    Durable,
    /// Answers are being passed through to the validator while the store
    /// builds or migrates. Coherent with the chain, but not evidence that the
    /// store holds anything.
    Passthrough,
}

/// The highest block the store can answer for, and where that answer comes
/// from.
///
/// Cheap and infallible: it is held in memory and updated on commit, so a
/// caller can bound a read against it without paying for a disk read first.
///
/// `tip` is `Option` because an empty store has no highest block, and that is
/// an ordinary state — a store is empty before it has written genesis, and the
/// difference between "empty" and "holds genesis" is exactly what the writer
/// branches on. A `Height` alone cannot express it: height zero is a real
/// block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreWatermark {
    /// The highest block the store can answer for, or `None` when empty.
    pub tip: Option<BlockRef>,
    /// Whether reads are served from committed data or passed through.
    pub provenance: Provenance,
}

impl StoreWatermark {
    /// A store holding nothing, serving from its own (empty) data.
    pub fn empty() -> Self {
        Self {
            tip: None,
            provenance: Provenance::Durable,
        }
    }

    /// Whether `height` is at or below the watermark, and so within the range
    /// this store answers for.
    pub fn covers(&self, height: zaino_primitives::types::Height) -> bool {
        self.tip.is_some_and(|tip| height <= tip.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{BlockHash, Height};

    fn at(height: u32) -> StoreWatermark {
        StoreWatermark {
            tip: Some(BlockRef {
                height: Height::try_from(height).expect("valid height"),
                hash: BlockHash::from([0; 32]),
            }),
            provenance: Provenance::Durable,
        }
    }

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("valid height")
    }

    /// An empty store covers nothing — including genesis.
    ///
    /// The tempting shortcut is to treat an empty store as "watermark zero",
    /// which makes it claim to hold the genesis block it has not written yet.
    #[test]
    fn an_empty_store_covers_nothing() {
        assert!(!StoreWatermark::empty().covers(h(0)));
    }

    #[test]
    fn coverage_is_inclusive_of_the_watermark() {
        assert!(at(100).covers(h(100)));
        assert!(at(100).covers(h(0)));
        assert!(!at(100).covers(h(101)));
    }

    #[test]
    fn capabilities_are_deduplicated_and_searchable() {
        let caps = StoreCapabilities::new(vec![
            StoreCapability::CompactBlocks,
            StoreCapability::Core,
            StoreCapability::CompactBlocks,
        ]);
        assert!(caps.contains(StoreCapability::Core));
        assert!(caps.contains(StoreCapability::CompactBlocks));
        assert!(!caps.contains(StoreCapability::TxOutSet));
        assert_eq!(caps.iter().count(), 2);
    }
}
