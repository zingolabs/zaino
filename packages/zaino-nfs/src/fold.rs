//! [`fold_block`]: one block through every enabled index, in dependency order (`nfs.md` §5)
//!
//! - The one place indexes meet: a new index = one line here

use std::sync::Arc;

use zaino_index_compact_block as compact_block;
use zaino_index_transparent_address as transparent_address;
use zaino_index_tree_state as tree_state;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_internal_value_balance as value_balance;
use zaino_persistence::{Changes, IndexKind, Layer, MapRead, SequenceRead};
use zaino_primitives::types::{Block, TreeSizeOutOfRange};
use zaino_sync::{Folds, PerIndex};

use crate::snapshot::Views;

/// Every index the NFS folds, in fold order
pub const INDEXES: [IndexKind; 5] = [
    IndexKind::ValueBalance,
    IndexKind::CompactBlock,
    IndexKind::BlockHash,
    IndexKind::TreeState,
    IndexKind::TransparentAddress,
];

/// One node's payload: what the final stream carries + each index's state as of the block
#[derive(Debug)]
pub(crate) struct Folded {
    pub(crate) folds: Arc<Folds>,
    pub(crate) layers: PerIndex<Layer>,
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

/// `block` folded by every index enabled in `parent`: each into a delta its parent layer opens,
/// each layer = parent's `.with(delta)`
pub(crate) fn fold_block<V: SequenceRead + MapRead>(
    parent: &Views<V>,
    block: &Block,
) -> Result<Folded, FoldError> {
    let mut folds = Folds::default();
    let mut layers = PerIndex::default();
    let open = |kind: IndexKind| parent.layer(kind).changes(block.at());
    let mut push = |kind: IndexKind, changes: Changes| {
        layers.insert(kind, parent.layer(kind).with(&changes));
        folds.insert(kind, changes);
    };

    if let Some(reader) = parent.value_balance() {
        let mut out = open(IndexKind::ValueBalance);
        let fees = value_balance::fold(&reader, block, &mut out)?;
        push(IndexKind::ValueBalance, out);
        if let Some(reader) = parent.compact_block() {
            let mut out = open(IndexKind::CompactBlock);
            compact_block::fold(&reader, block, &fees, &mut out)?;
            push(IndexKind::CompactBlock, out);
        }
    }
    if let Some(reader) = parent.block_hash() {
        let mut out = open(IndexKind::BlockHash);
        block_hash::fold(&reader, block, &mut out);
        push(IndexKind::BlockHash, out);
    }
    if let Some(reader) = parent.tree_state() {
        let mut out = open(IndexKind::TreeState);
        tree_state::fold(&reader, block, &mut out)?;
        push(IndexKind::TreeState, out);
    }
    if let Some(reader) = parent.transparent_address() {
        let mut out = open(IndexKind::TransparentAddress);
        transparent_address::fold(&reader, block, &mut out);
        push(IndexKind::TransparentAddress, out);
    }
    Ok(Folded { folds: Arc::new(folds), layers })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
    /// - disabled indexes: no `Changes`, no layer, no reader
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
                let store = engine.open(Path::new(kind.name()), &schema).expect("fresh store");
                durable.insert(kind, store.view());
                root.insert(kind, Layer::empty(&schema));
                stores.push(store);
            }
            let folded = fold_block(&Views::new(&durable, &root), genesis).expect("folds");
            let folded = fold_block(&Views::new(&durable, &folded.layers), block).expect("folds");
            let views = Views::new(&durable, &folded.layers);

            let reader = views.compact_block().expect("enabled");
            assert_eq!(
                reader.block(h1),
                Some(record.clone()),
                "{enabled:?}: block 1 = its fees + sizes"
            );
            for kind in all {
                let tips = (
                    folded.folds.get(kind).map(Changes::tip),
                    folded.layers.get(kind).map(Layer::tip),
                );
                let expected = match enabled.contains(&kind) {
                    true => (Some(one), Some(Some(one))),
                    false => (None, None),
                };
                assert_eq!(tips, expected, "{enabled:?}: {}", kind.name());
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
