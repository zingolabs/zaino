//! What a store can serve, how far it can serve it, and what schema it is on.

use core::fmt;

use zaino_primitives::types::BlockRef;

/// One thing a chain store may be able to answer.
///
/// Coarser than a method and finer than a trait bound: it names an *index*, on
/// the grounds that indexes are what a deployment chooses to build. A capability being absent is a fact about this store, not
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

impl StoreCapability {
    /// Every capability, ascending.
    ///
    /// The set is closed — one variant per index the finalised state maintains
    /// — so it can be enumerated. A store's advertised set is a subset of this,
    /// and a coherence check over the ports needs something to iterate.
    pub const ALL: [Self; 7] = [
        Self::Core,
        Self::StoredBlocks,
        Self::CompactBlocks,
        Self::Transactions,
        Self::SpentOutputs,
        Self::TxOutSet,
        Self::TransparentHistory,
    ];

    /// This capability's bit in a [`StoreCapabilities`] set.
    const fn bit(self) -> u8 {
        match self {
            Self::Core => 1 << 0,
            Self::StoredBlocks => 1 << 1,
            Self::CompactBlocks => 1 << 2,
            Self::Transactions => 1 << 3,
            Self::SpentOutputs => 1 << 4,
            Self::TxOutSet => 1 << 5,
            Self::TransparentHistory => 1 << 6,
        }
    }
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
/// Runtime state, not a type-level fact: which optional indexes a store holds
/// depends on how it was built.
/// A bit set rather than a sorted `Vec`. The capability set is closed and
/// small, so membership is a mask test; sorting, deduplicating and binary
/// searching a seven-element vector was more machinery — and an allocation —
/// for an answer a `u8` holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreCapabilities(u8);

impl StoreCapabilities {
    /// The set offering exactly these capabilities.
    ///
    /// Duplicates are absorbed rather than rejected: a set is a set, and the
    /// callers assembling one are testing independent conditions that may name
    /// the same capability twice.
    pub fn new(capabilities: impl IntoIterator<Item = StoreCapability>) -> Self {
        Self(
            capabilities
                .into_iter()
                .fold(0, |bits, capability| bits | capability.bit()),
        )
    }

    /// Whether this store currently offers `capability`.
    pub fn contains(&self, capability: StoreCapability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// Every capability offered, ascending.
    pub fn iter(&self) -> impl Iterator<Item = StoreCapability> + '_ {
        StoreCapability::ALL
            .into_iter()
            .filter(|capability| self.contains(*capability))
    }
}

/// The highest block the store can answer for.
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
}

impl StoreWatermark {
    /// A store holding nothing.
    pub fn empty() -> Self {
        Self { tip: None }
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
