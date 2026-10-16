//! [`fold_block`]: one block through each covered index, in [`IndexKind::ALL`] order (`usage.md`)

use crate::indexes::{fold, FoldError};
use crate::per_index::PerIndex;
use zaino_persistence::{AppliedLayer, IndexKind, MapRead, Overlay, SequenceRead};
use zaino_primitives::types::Block;

use crate::publish::Views;

/// One node's payload: each covered index's state as of the block
#[derive(Debug)]
pub(crate) struct Folded {
    pub(crate) layers: PerIndex<Overlay>,
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
    let mut fees = None;
    for &kind in IndexKind::ALL.iter().filter(|kind| covers.contains(kind)) {
        if kind == IndexKind::CompactBlock && fees.is_none() {
            let utxo_set = present(IndexKind::UtxoSet, parent.utxo_set());
            fees = Some(utxo_set.block_fees(block)?);
        }
        let layer = parent.layer(kind);
        let mut out = layer.schema().changes(block.block_ref());
        let folded = fold(kind, present(kind, parent.view(kind)), block, fees.as_ref(), &mut out)?;
        fees = folded.or(fees);
        layers.insert(kind, layer.with(&out));
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
    use zaino_primitives::types::{BlockFees, Fee, TreeSize, TreeSizes};
    use zaino_source::testing::{h, p2pkh, MockChain};
    use zcash_protocol::consensus::H0;
    use zcash_protocol::value::Zatoshis;

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
        for enabled in [all, &[UtxoSet, CompactBlock, TreeState]] {
            let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
            let (mut stores, mut durable, mut root) =
                (Vec::new(), PerIndex::default(), PerIndex::default());
            for &kind in enabled {
                let schema = crate::schema(kind, crate::tests::NETWORK);
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
            for &kind in all {
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
