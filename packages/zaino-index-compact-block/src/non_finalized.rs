//! Blocks applied above the finalised tip: the non-finalized tier
//!
//! - holds what is **applied**, never what is finalised: [`apply`](NonFinalizedState::apply)
//!   extends it, [`finalize_through`](NonFinalizedState::finalize_through) hands its root to the
//!   store, a reorg drops it whole (`= default()`, no reverse fold)
//! - only the winning chain reaches here (competing branches = the chain head's business)
//! - records pre-encoded by the store's own encoder (both tiers byte-identical)
//! - `imbl` throughout: a publication clones in `O(1)`, so the writer republishes every block

use bytes::Bytes;
use imbl::OrdMap;
use zaino_primitives::types::Height;

use crate::{project::project, Pools, HASH};

/// One applied block, as stored and as the default `GetBlockRange` wants it
///
/// - `shielded` = `record` projected to [`Pools::default`], once at apply (every synced wallet
///   asks each tip block in that shape); the same `Bytes` when nothing is pruned
#[derive(Debug, Clone)]
struct Held {
    hash: [u8; HASH],
    record: Bytes,
    shielded: Bytes,
}

/// Blocks above the finalised tip, along the one chain the chain head chose
#[derive(Debug, Clone, Default)]
pub(crate) struct NonFinalizedState {
    blocks: OrdMap<Height, Held>,
}

impl NonFinalizedState {
    /// One above the finalised tip (`None` = empty)
    pub(crate) fn root_height(&self) -> Option<Height> {
        self.blocks.get_min().map(|(height, _)| *height)
    }

    pub(crate) fn tip_height(&self) -> Option<Height> {
        self.blocks.get_max().map(|(height, _)| *height)
    }

    /// Tip height + hash (`None` = empty)
    pub(crate) fn tip_id(&self) -> Option<(Height, [u8; HASH])> {
        self.blocks.get_max().map(|(height, held)| (*height, held.hash))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Framed, wire-ready
    pub(crate) fn block(&self, height: Height) -> Option<Bytes> {
        self.blocks.get(&height).map(|held| held.record.clone())
    }

    /// Framed, projected to `pools`
    pub(crate) fn projected(&self, height: Height, pools: Pools) -> Option<Bytes> {
        let held = self.blocks.get(&height)?;
        Some(match pools {
            Pools::ALL => held.record.clone(),
            shielded if shielded == Pools::default() => held.shielded.clone(),
            other => project(&held.record, other).expect("a held record walked at apply"),
        })
    }

    pub(crate) fn hash_at(&self, height: Height) -> Option<[u8; HASH]> {
        self.blocks.get(&height).map(|held| held.hash)
    }

    /// Adds one block at the tip
    ///
    /// - `record` = this index's own encoding: a projection failing = an encoder bug
    pub(crate) fn apply(&mut self, height: Height, hash: [u8; HASH], record: Bytes) {
        if let Some(tip) = self.tip_height() {
            assert_eq!(height, tip.next(), "non-finalized apply out of order");
        }
        let shielded = project(&record, Pools::default())
            .expect("a record this index just encoded walks its own framing");
        // pruning only shrinks: same length = nothing dropped, share the record
        let shielded = if shielded.len() == record.len() { record.clone() } else { shielded };
        self.blocks.insert(height, Held { hash, record, shielded });
    }

    /// Drops every block at or below `height` (now the store's: one copy, one source)
    pub(crate) fn finalize_through(&mut self, height: Height) {
        self.blocks = self.blocks.split(&height).1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_compact_block, testing::block};

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// Applied blocks resolve by height in every shape (the default projection precomputed and
    /// byte-identical to projecting on read), and finalising hands the root over
    #[test]
    fn applied_blocks_resolve_by_height_and_finalising_hands_over_the_root() {
        let mut state = NonFinalizedState::default();
        assert_eq!((state.root_height(), state.tip_id()), (None, None));

        let records: Vec<Bytes> = (100..103u32)
            .map(|height| {
                let (block, balances, sizes) = block(height);
                encode_compact_block(&block, &balances, &sizes)
            })
            .collect();
        for (n, record) in (100..103u8).zip(&records) {
            state.apply(h(n.into()), [n; HASH], record.clone());
        }
        assert_eq!(
            (state.root_height(), state.tip_id()),
            (Some(h(100)), Some((h(102), [102; HASH])))
        );
        assert_eq!(state.block(h(101)), Some(records[1].clone()));
        assert_eq!(state.hash_at(h(101)), Some([101; HASH]));

        let sapling_only = Pools { orchard: false, ironwood: false, ..Pools::default() };
        for pools in [Pools::ALL, Pools::default(), sapling_only] {
            let on_read = project(&records[1], pools);
            assert_eq!(state.projected(h(101), pools), on_read, "{pools:?}");
        }
        let full = state.projected(h(101), Pools::ALL).expect("held");
        let shielded = state.projected(h(101), Pools::default()).expect("held");
        assert!(shielded.len() < full.len(), "the fixture carries transparent data to prune");
        assert_eq!(state.projected(h(99), Pools::ALL), None, "not held");

        state.finalize_through(h(100));
        assert_eq!((state.root_height(), state.tip_height()), (Some(h(101)), Some(h(102))));
        let handed_over = (state.block(h(100)), state.hash_at(h(100)));
        assert_eq!(handed_over, (None, None), "a handed-over block is unreachable");
    }

    #[test]
    #[should_panic(expected = "non-finalized apply out of order")]
    fn a_gap_panics() {
        let mut state = NonFinalizedState::default();
        let record = |height| {
            let (block, balances, sizes) = block(height);
            encode_compact_block(&block, &balances, &sizes)
        };
        state.apply(h(5), [5; HASH], record(5));
        state.apply(h(7), [7; HASH], record(7));
    }
}
