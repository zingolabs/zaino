//! [`fold_block`]: one block through every enabled index, in dependency order (`nfs.md` §5)
//!
//! - The one place indexes meet: a new index = one line here + one in [`schema`]

use std::sync::Arc;

use zaino_index_compact_block as compact_block;
use zaino_index_transparent_address as transparent_address;
use zaino_index_tree_state as tree_state;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_internal_value_balance as value_balance;
use zaino_persistence::{Changes, IndexKind, Layer, MapRead, Schema, SequenceRead};
use zaino_primitives::types::{Block, TreeSizeOutOfRange};
use zaino_sync::{Folds, PerIndex};
use zcash_protocol::consensus::NetworkType;

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

/// `block` folded by every index enabled in `parent`, each layer = parent's `.with(own Changes)`
pub(crate) fn fold_block<V: SequenceRead + MapRead>(
    parent: &Views<V>,
    block: &Block,
) -> Result<Folded, FoldError> {
    let mut folds = Folds::default();
    let mut layers = PerIndex::default();
    let mut push = |kind: IndexKind, changes: Changes| {
        layers.insert(kind, parent.layer(kind).with(&changes));
        folds.insert(kind, changes);
    };

    if let Some(reader) = parent.value_balance() {
        let (changes, fees) = value_balance::fold(&reader, block)?;
        push(IndexKind::ValueBalance, changes);
        if let Some(reader) = parent.compact_block() {
            push(IndexKind::CompactBlock, compact_block::fold(&reader, block, &fees)?);
        }
    }
    if parent.enabled(IndexKind::BlockHash) {
        push(IndexKind::BlockHash, block_hash::fold(block, parent.network()));
    }
    if let Some(reader) = parent.tree_state() {
        push(IndexKind::TreeState, tree_state::fold(&reader, block)?);
    }
    if let Some(reader) = parent.transparent_address() {
        push(IndexKind::TransparentAddress, transparent_address::fold(&reader, block));
    }
    Ok(Folded { folds: Arc::new(folds), layers })
}

/// `kind`'s tables, as its crate declares them
pub fn schema(kind: IndexKind, network: NetworkType) -> Schema {
    match kind {
        IndexKind::ValueBalance => value_balance::schema(network),
        IndexKind::CompactBlock => compact_block::schema(network),
        IndexKind::BlockHash => block_hash::schema(network),
        IndexKind::TreeState => tree_state::schema(network),
        IndexKind::TransparentAddress => transparent_address::schema(network),
        IndexKind::HeaderChain => zaino_header_chain::schema(network),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store, View};
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{
        BlockFees, CompactCiphertext, Fee, Height, OrchardAction, OrchardData, OutPoint,
        SaplingData, SaplingOutput, Script, Transaction, TransactionId, TransparentData,
        TransparentOutput, TreeSize, TreeSizes, Zatoshis,
    };

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
        let network = NetworkType::Regtest;
        let p2pkh = Script::new([&[0x76, 0xa9, 0x14][..], &[0xaa; 20], &[0x88, 0xac]].concat());
        let pays = |value: u64| TransparentOutput {
            value: Zatoshis::new(value).expect("in supply"),
            script: p2pkh.clone(),
        };
        let coinbase = |tag: u8, value: u64| Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
                coinbase: true,
                inputs: vec![],
                outputs: vec![pays(value)],
            },
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        let spend = Transaction {
            txid: TransactionId::from([0x20; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: vec![OutPoint { txid: TransactionId::from([0x10; 32]), vout: 0 }],
                outputs: vec![pays(49_000)],
            },
            sprout: Default::default(),
            sapling: SaplingData {
                outputs: vec![SaplingOutput {
                    cmu: [0x01; 32].into(),
                    ephemeral_key: [0x02; 32].into(),
                    enc_ciphertext: CompactCiphertext::from([0x03; CompactCiphertext::LENGTH]),
                }],
                ..Default::default()
            },
            orchard: OrchardData {
                actions: vec![OrchardAction {
                    nullifier: [0x04; 32].into(),
                    cmx: [0x01; 32].into(),
                    ephemeral_key: [0x06; 32].into(),
                    enc_ciphertext: CompactCiphertext::from([0x07; CompactCiphertext::LENGTH]),
                }],
                ..Default::default()
            },
            ironwood: Default::default(),
        };
        let mut chain = Chain::with_genesis(vec![coinbase(0x10, 50_000)]);
        let one = chain.mine_with(chain.genesis().hash, vec![coinbase(0x11, 10_000), spend]);
        let (genesis, block) = (chain.block(chain.genesis().hash), chain.block(one.hash));
        let h1 = Height::try_from(1u32).expect("h");
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
                let schema = schema(kind, network);
                let store = engine.open(Path::new(kind.name()), &schema).expect("fresh store");
                durable.insert(kind, store.view());
                root.insert(kind, Layer::empty(&schema));
                stores.push(store);
            }
            let folded = fold_block(&Views::new(network, &durable, &root), genesis).expect("folds");
            let folded =
                fold_block(&Views::new(network, &durable, &folded.layers), block).expect("folds");
            let views = Views::new(network, &durable, &folded.layers);

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
