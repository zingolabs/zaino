//! [`fold_block`]: one block through each covered index, in [`IndexKind::ALL`] order (`usage.md`)

use crate::indexes::{fold, FoldError};
use crate::per_index::{IndexSet, PerIndex};
use zaino_persistence::{AppliedLayer, IndexKind, MapRead, Overlay, SequenceRead};
use zaino_primitives::types::Block;

use crate::publish::Views;

/// One node's payload: each covered index's state as of the block
#[derive(Debug)]
pub(crate) struct Folded {
    pub(crate) layers: PerIndex<Overlay>,
}

/// `block` folded by each of `parent`'s indexes in `covers`: each into a delta its parent layer
/// opens, each layer = parent's `.with(delta)`
///
/// - compact-block's fees: utxo-set's fold, else (utxo-set durable at `block`) its
///   stored fees record
/// - panics: compact-block covered, utxo-set disabled
pub(crate) fn fold_block<V: SequenceRead + MapRead>(
    parent: &Views<V>,
    block: &Block,
    covers: IndexSet,
) -> Result<Folded, FoldError> {
    let mut layers = PerIndex::default();
    let mut fees = None;
    for (kind, layer, view) in parent.each().filter(|(kind, ..)| covers.contains(*kind)) {
        if kind == IndexKind::CompactBlock && fees.is_none() {
            let utxo_set = parent.utxo_set().expect("compact-block folds with utxo-set enabled");
            fees = Some(utxo_set.block_fees(block)?);
        }
        let mut out = layer.schema().changes(block.block_ref());
        let folded = fold(kind, view, block, fees.as_ref(), &mut out)?;
        fees = folded.or(fees);
        layers.insert(kind, layer.with(&out));
    }
    Ok(Folded { layers })
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store, View};
    use zaino_primitives::types::{BlockFees, Fee, TreeSize, TreeSizes};
    use zaino_source::testing::{h, p2pkh, MockChain};
    use zcash_protocol::consensus::H0;
    use zcash_protocol::value::Zatoshis;

    use super::*;

    /// Genesis (a 50 000 coinbase), then block 1 (a coinbase + a spend of it paying 1 000 in fees,
    /// with one sapling output and one orchard action), both folded by every index
    ///
    /// - each layer = parent layer + the block (tip = block 1, genesis read through it)
    /// - compact-block's record carries utxo-set's fees (utxo-set folded first)
    #[test]
    fn every_index_folds_the_block_onto_its_parent_layer_compact_block_with_utxo_set_fees() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain =
            MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
        let funding = chain.output(chain.genesis(), 0, 0);
        let one = chain.mine(|b| {
            b.coinbase(|c| c.pay(&alice, 10_000)).tx(|t| {
                t.spend(funding)
                    .pay(&alice, 49_000)
                    .fee(1_000)
                    .sapling_output(1)
                    .orchard_action(0x04, 1)
            })
        });
        let (genesis, block) = (chain.block(chain.genesis().hash), chain.block(one.hash));
        let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
        let (mut stores, mut durable) = (Vec::new(), PerIndex::default());
        for kind in IndexKind::ALL {
            let schema = crate::schema(kind, crate::nfs::tests::NETWORK);
            let store = engine
                .open(Path::new(kind.name()), &schema, NonZeroUsize::MAX)
                .expect("fresh store");
            durable.insert(kind, store.durable());
            stores.push(store);
        }

        let every: IndexSet = IndexKind::ALL.into_iter().collect();
        let root = Views::at(&durable, &PerIndex::default(), None);
        let folded = fold_block(&root, genesis, every).expect("folds");
        let parent = Views::at(&durable, &folded.layers, Some(H0));
        let folded = fold_block(&parent, block, every).expect("folds");

        let views = Views::at(&durable, &folded.layers, Some(h(1)));
        for kind in IndexKind::ALL {
            let tip = folded.layers.get(kind).map(Overlay::tip);
            assert_eq!(tip, Some(Some(one)), "{}: layer tip = block 1", kind.name());
        }
        let fees = BlockFees {
            height: h(1),
            hash: one.hash,
            fees: vec![Fee::Coinbase, Fee::Paid(Zatoshis::from_u64(1_000).expect("in supply"))],
        };
        let sizes = TreeSizes {
            sapling: TreeSize::from(1),
            orchard: TreeSize::from(1),
            ironwood: TreeSize::from(0),
        };
        let record = zaino_index_compact_block::encode_compact_block(block, &fees, &sizes);
        let compact_block = views.compact_block().expect("enabled").block(h(1));
        assert_eq!(compact_block, Some(record), "compact-block 1 = utxo-set's fees + tree sizes");
        let block_hash = views.block_hash().expect("enabled");
        let located = [chain.genesis().hash, one.hash].map(|hash| block_hash.height_of(&hash));
        assert_eq!(
            located,
            [Some(H0), Some(h(1))],
            "block-hash: genesis and 1 through both layers"
        );
    }

    /// Same two blocks, only utxo-set, compact-block and tree-state enabled
    #[test]
    fn disabled_indexes_get_no_layer_and_no_reader() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain =
            MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
        let funding = chain.output(chain.genesis(), 0, 0);
        let one = chain.mine(|b| {
            b.coinbase(|c| c.pay(&alice, 10_000)).tx(|t| {
                t.spend(funding)
                    .pay(&alice, 49_000)
                    .fee(1_000)
                    .sapling_output(1)
                    .orchard_action(0x04, 1)
            })
        });
        let (genesis, block) = (chain.block(chain.genesis().hash), chain.block(one.hash));
        let enabled = [IndexKind::UtxoSet, IndexKind::CompactBlock, IndexKind::TreeState];
        let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
        let (mut stores, mut durable) = (Vec::new(), PerIndex::default());
        for kind in enabled {
            let schema = crate::schema(kind, crate::nfs::tests::NETWORK);
            let store = engine
                .open(Path::new(kind.name()), &schema, NonZeroUsize::MAX)
                .expect("fresh store");
            durable.insert(kind, store.durable());
            stores.push(store);
        }

        let covers: IndexSet = enabled.into_iter().collect();
        let root = Views::at(&durable, &PerIndex::default(), None);
        let folded = fold_block(&root, genesis, covers).expect("folds");
        let parent = Views::at(&durable, &folded.layers, Some(H0));
        let folded = fold_block(&parent, block, covers).expect("folds");

        let views = Views::at(&durable, &folded.layers, Some(h(1)));
        let layers: Vec<&str> = folded.layers.iter().map(|(kind, _)| kind.name()).collect();
        assert_eq!(layers, ["utxo_set", "compact_block", "tree_state"], "one layer per enabled");
        let readers = [
            ("block_hash", views.block_hash().is_some()),
            ("transparent_address", views.transparent_address().is_some()),
            ("compact_block", views.compact_block().is_some()),
        ];
        let expected =
            [("block_hash", false), ("transparent_address", false), ("compact_block", true)];
        assert_eq!(readers, expected, "(reader, present)");
        let tree_state = views.view(IndexKind::TreeState).map(|view| view.tip());
        assert_eq!(tree_state, Some(Some(one)), "tree-state read through block 1's layer");
    }
}
