//! [`fold_block`]: one block through each covered index, in dependency order (`nfs.md`)
//!
//! - The one place indexes meet: a new index = one line here

use zaino_index_compact_block as compact_block;
use zaino_index_transparent_address as transparent_address;
use zaino_index_tree_state as tree_state;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_internal_value_balance as value_balance;
use zaino_persistence::{BlockChanges, IndexKind, MapRead, Overlay, SequenceRead};
use zaino_primitives::types::{Block, TreeSizeOutOfRange};
use zaino_sync::PerIndex;

use crate::snapshot::Views;

/// Every index the NFS folds, in fold order
pub const INDEXES: [IndexKind; 5] = [
    IndexKind::ValueBalance,
    IndexKind::CompactBlock,
    IndexKind::BlockHash,
    IndexKind::TreeState,
    IndexKind::TransparentAddress,
];

/// One node's payload: each covered index's state as of the block
#[derive(Debug)]
pub(crate) struct Folded {
    pub(crate) layers: PerIndex<Overlay>,
}

#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error("value_balance: {0}")]
    ValueBalance(#[from] value_balance::FoldError),
    #[error("compact_block: {0}")]
    CompactBlock(#[from] TreeSizeOutOfRange),
    #[error("tree_state: {0}")]
    TreeState(#[from] tree_state::FoldError),
}

/// `block` folded by each index in `covers`: each into a delta its parent layer opens, each layer =
/// parent's `.with(delta)`
///
/// - compact-block's fees: value-balance's fold, else (value-balance durable at `block`) its view
/// - panics: a covered index absent from `parent` (compact-block: value-balance too)
pub(crate) fn fold_block<V: SequenceRead + MapRead>(
    parent: &Views<V>,
    block: &Block,
    covers: &[IndexKind],
) -> Result<Folded, FoldError> {
    let mut layers = PerIndex::default();
    let open = |kind: IndexKind| parent.layer(kind).changes(block.at());
    let mut push = |kind: IndexKind, changes: BlockChanges| {
        layers.insert(kind, parent.layer(kind).with(&changes))
    };
    let covered = |kind: IndexKind| covers.contains(&kind);

    let mut fees = None;
    if covered(IndexKind::ValueBalance) {
        let reader = parent.value_balance().unwrap_or_else(|| absent(IndexKind::ValueBalance));
        let mut out = open(IndexKind::ValueBalance);
        fees = Some(value_balance::fold(&reader, block, &mut out)?);
        push(IndexKind::ValueBalance, out);
    }
    if covered(IndexKind::CompactBlock) {
        let fees = match fees {
            Some(fees) => fees,
            None => {
                let reader =
                    parent.value_balance().unwrap_or_else(|| absent(IndexKind::ValueBalance));
                let mut fees = value_balance::fees(&reader, &[block])?;
                fees.pop().expect("one block in, one fee set out")
            }
        };
        let reader = parent.compact_block().unwrap_or_else(|| absent(IndexKind::CompactBlock));
        let mut out = open(IndexKind::CompactBlock);
        compact_block::fold(&reader, block, &fees, &mut out)?;
        push(IndexKind::CompactBlock, out);
    }
    if covered(IndexKind::BlockHash) {
        let reader = parent.block_hash().unwrap_or_else(|| absent(IndexKind::BlockHash));
        let mut out = open(IndexKind::BlockHash);
        block_hash::fold(&reader, block, &mut out);
        push(IndexKind::BlockHash, out);
    }
    if covered(IndexKind::TreeState) {
        let reader = parent.tree_state().unwrap_or_else(|| absent(IndexKind::TreeState));
        let mut out = open(IndexKind::TreeState);
        tree_state::fold(&reader, block, &mut out)?;
        push(IndexKind::TreeState, out);
    }
    if covered(IndexKind::TransparentAddress) {
        let reader =
            parent.transparent_address().unwrap_or_else(|| absent(IndexKind::TransparentAddress));
        let mut out = open(IndexKind::TransparentAddress);
        transparent_address::fold(&reader, block, &mut out);
        push(IndexKind::TransparentAddress, out);
    }
    Ok(Folded { layers })
}

fn absent(kind: IndexKind) -> ! {
    panic!("{}: covered, absent from the fold parent", kind.name())
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store, View};
    use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain};
    use zaino_primitives::types::{BlockFees, Fee, Height, TreeSize, TreeSizes, Zatoshis};

    use super::*;

    /// Genesis (a 50 000 coinbase), then block 1 (a coinbase + a spend of it paying 1 000 in fees,
    /// with one sapling output and one orchard action), folded twice by `fold_block`: once with
    /// every index, once with block-hash and transparent-address disabled
    ///
    /// - compact-block's record carries value-balance's fees (value-balance folded first)
    /// - each layer = parent layer + the block (tip = block 1, genesis read through it)
    /// - disabled indexes: no layer, no reader
    #[test]
    fn value_balance_folds_first_and_disabled_indexes_fold_nothing() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 50_000)));
        let one = chain.mine(|b| {
            b.coinbase(|c| c.pay(&alice, 10_000)).tx(|t| {
                t.spend(outpoint([0x10; 32], 0))
                    .pay(&alice, 49_000)
                    .fee(1_000)
                    .sapling_output(1)
                    .orchard_action([0x04; 32], 1)
            })
        });
        let (genesis, block) = (chain.block(chain.genesis().hash), chain.block(one.hash));
        let h1 = h(1);
        let fees = BlockFees {
            height: h1,
            hash: one.hash,
            fees: vec![Fee::Coinbase, Fee::Paid(Zatoshis::new(1_000).expect("in supply"))],
        };
        let sizes = TreeSizes {
            sapling: TreeSize::from(1),
            orchard: TreeSize::from(1),
            ironwood: TreeSize::from(0),
        };
        let record = compact_block::encode_compact_block(block, &fees, &sizes);

        use IndexKind::*;
        let all = INDEXES;
        for enabled in [&all[..], &[ValueBalance, CompactBlock, TreeState]] {
            let engine = DiskEngine::new(SimFs::new());
            let (mut stores, mut durable, mut root) =
                (Vec::new(), PerIndex::default(), PerIndex::default());
            for &kind in enabled {
                let schema = crate::tests::schema(kind);
                let store = engine
                    .open(Path::new(kind.name()), &schema, NonZeroUsize::MAX)
                    .expect("fresh store");
                durable.insert(kind, store.committed());
                root.insert(kind, Overlay::empty(&schema));
                stores.push(store);
            }
            let folded = fold_block(&Views::new(&durable, &root), genesis, enabled).expect("folds");
            let folded =
                fold_block(&Views::new(&durable, &folded.layers), block, enabled).expect("folds");
            let views = Views::new(&durable, &folded.layers);

            let reader = views.compact_block().expect("enabled");
            assert_eq!(
                reader.block(h1),
                Some(record.clone()),
                "{enabled:?}: block 1 = its fees + sizes"
            );
            for kind in all {
                let tip = folded.layers.get(kind).map(Overlay::tip);
                let expected = enabled.contains(&kind).then_some(Some(one));
                assert_eq!(tip, expected, "{enabled:?}: {}", kind.name());
            }
            let located = views
                .block_hash()
                .map(|reader| [chain.genesis().hash, one.hash].map(|hash| reader.height_of(&hash)));
            let expected =
                enabled.contains(&BlockHash).then_some([Some(Height::GENESIS), Some(h1)]);
            assert_eq!(located, expected, "{enabled:?}: block-hash through both layers");
            assert_eq!(
                views.transparent_address().is_some(),
                enabled.contains(&TransparentAddress)
            );
            assert_eq!(views.view(TreeState).map(|view| view.tip()), Some(Some(one)));
        }
    }
}
