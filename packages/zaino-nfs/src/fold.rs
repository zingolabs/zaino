//! [`fold_block`]: one block through each covered index, in dependency order (`usage.md`)
//!
//! - The one place indexes meet: a new index = one fold call here

use zaino_persistence::PerIndex;
use zaino_persistence::{BlockChanges, IndexKind, MapRead, Overlay, SequenceRead, Uncommitted};
use zaino_primitives::types::Block;

use crate::publish::Views;

/// One node's payload: each covered index's state as of the block
#[derive(Debug)]
pub(crate) struct Folded {
    pub(crate) layers: PerIndex<Overlay>,
}

#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error("utxo_set: {0}")]
    UtxoSet(#[from] zaino_index_utxo_set::FoldError),
    #[error("tree_state: {0}")]
    TreeState(#[from] zaino_index_tree_state::FoldError),
}

/// `block` folded by each index in `covers`: each into a delta its parent layer opens, each layer =
/// parent's `.with(delta)`
///
/// - compact-block's fees: utxo-set's fold, else (utxo-set durable at `block`) its
///   stored fees record
/// - panics: a covered index absent from `parent` (compact-block: utxo-set too)
pub(crate) fn fold_block<V: SequenceRead + MapRead>(
    parent: &Views<V>,
    block: &Block,
    covers: &[IndexKind],
) -> Result<Folded, FoldError> {
    let mut layers = PerIndex::default();
    let open = |kind: IndexKind| parent.layer(kind).schema().changes(block.at());
    let mut push = |kind: IndexKind, changes: BlockChanges| {
        layers.insert(kind, parent.layer(kind).with(&changes))
    };
    let covered = |kind: IndexKind| covers.contains(&kind);

    let mut fees = None;
    if covered(IndexKind::UtxoSet) || covered(IndexKind::CompactBlock) {
        let reader = present(IndexKind::UtxoSet, parent.utxo_set());
        fees = Some(match covered(IndexKind::UtxoSet) {
            true => {
                let mut out = open(IndexKind::UtxoSet);
                let fees = zaino_index_utxo_set::fold(&reader, block, &mut out)?;
                push(IndexKind::UtxoSet, out);
                fees
            }
            false => reader.block_fees(block)?,
        });
    }
    if let Some(fees) = fees.filter(|_| covered(IndexKind::CompactBlock)) {
        let reader = present(IndexKind::CompactBlock, parent.compact_block());
        let mut out = open(IndexKind::CompactBlock);
        zaino_index_compact_block::fold(&reader, block, &fees, &mut out);
        push(IndexKind::CompactBlock, out);
    }
    if covered(IndexKind::BlockHash) {
        let reader = present(IndexKind::BlockHash, parent.block_hash());
        let mut out = open(IndexKind::BlockHash);
        zaino_internal_block_hash_to_height::fold(&reader, block, &mut out);
        push(IndexKind::BlockHash, out);
    }
    if covered(IndexKind::TreeState) {
        let reader = present(IndexKind::TreeState, parent.tree_state());
        let mut out = open(IndexKind::TreeState);
        zaino_index_tree_state::fold(&reader, block, &mut out)?;
        push(IndexKind::TreeState, out);
    }
    if covered(IndexKind::TransparentAddress) {
        let reader = present(IndexKind::TransparentAddress, parent.transparent_address());
        let mut out = open(IndexKind::TransparentAddress);
        zaino_index_transparent_address::fold(&reader, block, &mut out);
        push(IndexKind::TransparentAddress, out);
    }
    Ok(Folded { layers })
}

/// Covered `kind`'s reader from the fold parent (absent = a caller bug: panics)
fn present<R>(kind: IndexKind, reader: Option<R>) -> R {
    reader.unwrap_or_else(|| panic!("{}: covered, absent from the fold parent", kind.name()))
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store, View};
    use zaino_primitives::testing::{h, p2pkh, MockChain};
    use zaino_primitives::types::{BlockFees, Fee, TreeSize, TreeSizes};
    use zcash_protocol::consensus::H0;
    use zcash_protocol::value::Zatoshis;
    use zcash_transparent::bundle::OutPoint;

    use super::*;

    /// Genesis (a 50 000 coinbase), then block 1 (a coinbase + a spend of it paying 1 000 in fees,
    /// with one sapling output and one orchard action), folded twice by `fold_block`: once with
    /// every index, once with block-hash and transparent-address disabled
    ///
    /// - compact-block's record carries utxo-set's fees (utxo-set folded first)
    /// - each layer = parent layer + the block (tip = block 1, genesis read through it)
    /// - disabled indexes: no layer, no reader
    #[test]
    fn utxo_set_folds_first_and_disabled_indexes_fold_nothing() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 50_000)));
        let one = chain.mine(|b| {
            b.coinbase(|c| c.pay(&alice, 10_000)).tx(|t| {
                t.spend(OutPoint::new([0x10; 32], 0))
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
            fees: vec![Fee::Coinbase, Fee::Paid(Zatoshis::from_u64(1_000).expect("in supply"))],
        };
        let sizes = TreeSizes {
            sapling: TreeSize::from(1),
            orchard: TreeSize::from(1),
            ironwood: TreeSize::from(0),
        };
        let record = zaino_index_compact_block::encode_compact_block(block, &fees, &sizes);

        use IndexKind::*;
        let all = IndexKind::ALL;
        for enabled in [&all[..], &[UtxoSet, CompactBlock, TreeState]] {
            let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
            let (mut stores, mut durable, mut root) =
                (Vec::new(), PerIndex::default(), PerIndex::default());
            for &kind in enabled {
                let schema = crate::tests::schema(kind);
                let store = engine
                    .open(Path::new(kind.name()), &schema, NonZeroUsize::MAX)
                    .expect("fresh store");
                durable.insert(kind, store.durable());
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
            let expected = enabled.contains(&BlockHash).then_some([Some(H0), Some(h1)]);
            assert_eq!(located, expected, "{enabled:?}: block-hash through both layers");
            assert_eq!(
                views.transparent_address().is_some(),
                enabled.contains(&TransparentAddress)
            );
            assert_eq!(views.view(TreeState).map(|view| view.tip()), Some(Some(one)));
        }
    }
}
