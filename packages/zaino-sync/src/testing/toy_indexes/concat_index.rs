//! BlockLocal × Monoidal, non-commutative: concatenates each batch's block
//! heights in chain order.
//!
//! Unlike [`count_index`](super::count_index) (integer addition, commutative),
//! this toy's `combine` is string concatenation — `A` before `B` differs from
//! `B` before `A`. It therefore detects a `LocalBridge` that folds deltas in
//! rayon completion order instead of chain order, which a commutative combine
//! hides. Each batch collapses to one entry keyed by the batch's first height.

use super::{ConcatAcc, PersistentConcat};
use crate::descriptor::{BlockLocal, Monoidal};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{ExtractLocal, IndexDef, MergeMonoidal, Schema};
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{EntryCodec, KeyOrder};

/// Block context for this index: just the block's height.
pub struct Context {
    /// Block height — the token this index concatenates.
    pub height: BlockHeight,
}

/// Concatenates each batch's block heights in chain order.
pub struct ConcatIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("concat");

impl IndexDef for ConcatIndex {
    type Scope = BlockLocal;
    type Composition = Monoidal;
    type Delta = BlockHeight;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractLocal for ConcatIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &Context) -> Result<Self::Delta, Self::Error> {
        Ok(ctx.height)
    }
}

impl MergeMonoidal for ConcatIndex {
    type Accumulator = ConcatAcc;

    fn identity() -> Self::Accumulator {
        ConcatAcc::empty()
    }

    fn lift(delta: Self::Delta) -> Self::Accumulator {
        ConcatAcc::singleton(delta)
    }

    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator {
        a.followed_by(b)
    }
}

impl Schema<ConcatAcc> for ConcatIndex {
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

impl EntryCodec for ConcatIndex {
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
    use super::ConcatIndex;

    /// Under parallel (here: forced out-of-order) extraction, the monoidal merge
    /// still yields the heights concatenated in chain order. Fails on a bridge
    /// that folds in completion order.
    #[test]
    fn monoidal_merge_follows_chain_order_under_parallel_extraction() {
        for _ in 0..20 {
            let merged = run_toy_sync::<ConcatIndex>(50, 7);
            assert_eq!(
                merged,
                (0..50).map(|h| h.to_string()).collect::<Vec<_>>().join(",")
            );
        }
    }
}
