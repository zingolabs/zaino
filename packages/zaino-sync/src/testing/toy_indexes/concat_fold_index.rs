//! BlockLocal × Fold, non-commutative: concatenates each batch's block heights
//! in chain order.
//!
//! The `Fold` counterpart of [`concat_index`](super::concat_index). `Fold` is
//! documented order-dependent, yet shares [`LocalBridge`] with the other
//! BlockLocal compositions; the only other `Fold` toy
//! ([`running_sum_index`](super::running_sum_index)) uses `+=`, which is
//! commutative and so masks a merge that applies deltas out of chain order.
//! This toy's fold is string concatenation, so it catches it. Each batch
//! collapses to one entry keyed by the batch's first height.
//!
//! [`LocalBridge`]: crate::bridge

use super::{ConcatAcc, PersistentConcat};
use crate::descriptor::{BlockLocal, Fold};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractLocal, IndexDef, MergeFold, Schema};
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{EntryCodec, KeyOrder};

/// Block context for this index: just the block's height.
pub struct Context {
    /// Block height — the token this index concatenates.
    pub height: BlockHeight,
}

/// Concatenates each batch's block heights in chain order via an
/// order-dependent fold.
pub struct ConcatFoldIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("concat_fold");

impl IndexDef for ConcatFoldIndex {
    type Scope = BlockLocal;
    type Composition = Fold;
    type Delta = BlockHeight;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractLocal for ConcatFoldIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context) -> Result<Self::Delta, Self::Error> {
        Ok(ctx.height)
    }
}

impl MergeFold for ConcatFoldIndex {
    type FoldState = ConcatAcc;

    fn initial_state() -> Self::FoldState {
        ConcatAcc::empty()
    }

    fn fold(state: &mut Self::FoldState, delta: Self::Delta) {
        state.push_height(delta);
    }
}

impl Schema<ConcatAcc> for ConcatFoldIndex {
    fn into_entries(acc: ConcatAcc) -> Vec<(Self::Key, Self::Value)> {
        match acc.first() {
            Some(first) => vec![(first, acc.into_text())],
            None => Vec::new(),
        }
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> ConcatAcc {
        ConcatAcc::from_entries(entries)
    }
}

impl EntryCodec for ConcatFoldIndex {
    type Key = BlockHeight;
    type Value = String;
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentConcat;

    // Keyed by each batch's first height; batches are consumed in chain order,
    // so keys are appended strictly increasing.
    const KEY_ORDER: KeyOrder = KeyOrder::WalkOrdered;

    fn fingerprint_samples() -> Vec<(BlockHeight, String)> {
        vec![(BlockHeight::new(1), "1".to_owned())]
    }
}

#[cfg(test)]
mod tests {
    use super::super::run_toy_sync;
    use super::ConcatFoldIndex;

    /// Under parallel (here: forced out-of-order) extraction, the fold merge
    /// still yields the heights concatenated in chain order. Fails on a bridge
    /// that folds in completion order.
    #[test]
    fn fold_merge_follows_chain_order_under_parallel_extraction() {
        for _ in 0..20 {
            let merged = run_toy_sync::<ConcatFoldIndex>(50, 7);
            assert_eq!(
                merged,
                (0..50).map(|h| h.to_string()).collect::<Vec<_>>().join(",")
            );
        }
    }
}
