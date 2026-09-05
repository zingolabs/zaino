//! ChainIndex's side of the ChainHead boundary.
//!
//! Two things live here: how ChainIndex hands ChainHead a validator, and how a
//! [`ChainHeadBlock`] becomes the [`IndexedBlock`] the rest of this crate is
//! written against. Only the ChainHead-specific half of that conversion is
//! here — the field mapping belongs to the store that owns the shape, and is
//! shared rather than copied.
//!
//! # The conversion is temporary
//!
//! `IndexedBlock` is the finalised state's persisted shape, and ChainIndex
//! reads both halves of the chain through it. Converting at this boundary is
//! what lets ChainHead be extracted without rewriting every query path at the
//! same time. It goes away when ChainIndex is reworked to read the two halves
//! through their own vocabularies.
//!
//! Until then, one thing about the result is load-bearing: its `chainwork` is
//! **anchor-relative**, because ChainHead accumulates from its own window
//! rather than from genesis (see [`ChainHeadWork`]). Blocks produced here are
//! served, never persisted — the finalised state syncs from the validator
//! independently and computes absolute chainwork itself. Writing one of these
//! to the database would put a wrong chainwork on disk.

use std::sync::Arc;

use crate::chain_index::{
    source::BlockchainSource, source_ports::ChainIndexSourcePorts, types::ChainWork,
    validator_source::ValidatorSource,
};
use crate::IndexedBlock;
use zaino_chain_head::{ChainHeadBlock, ChainHeadBlockSource, ChainHeadWork};

/// A source that can also answer ChainHead's questions.
///
/// ChainHead speaks the `zaino-source` ports directly, while ChainIndex still
/// consumes the wire-typed [`BlockchainSource`] scaffolding. This trait is how
/// the second hands over the first: an implementor exposes the underlying
/// validator, and ChainHead is built on that rather than on the wrapper.
///
/// Kept off `BlockchainSource` because that port is frozen scaffolding
/// (docs/adr/0008) and shrinks as each subsystem moves onto the real ports.
pub trait WithChainHeadSource: BlockchainSource {
    /// The validator ChainHead will drive.
    type Head: ChainHeadBlockSource;

    /// The underlying validator, shared rather than cloned.
    fn chain_head_source(&self) -> Arc<Self::Head>;
}

/// A `ValidatorSource` offers a ChainHead source exactly when the validator it
/// wraps can answer ChainHead's questions.
///
/// The second bound is not redundant with the first: `ChainIndexSourcePorts`
/// names what ChainIndex asks — which includes the *raw* block ports, because
/// it builds its own index from the bytes — while ChainHead asks for parsed
/// blocks. Requiring both here keeps each bound describing one consumer's
/// needs, rather than restating ChainHead's inside ChainIndex's.
impl<V> WithChainHeadSource for ValidatorSource<V>
where
    V: ChainIndexSourcePorts + ChainHeadBlockSource,
{
    type Head = V;

    fn chain_head_source(&self) -> Arc<Self::Head> {
        self.validator()
    }
}

/// This crate's block hash, as the domain names it.
///
/// The two are the same 32 bytes; only the type differs, and only until this
/// crate's own primitives are retired in favour of the domain's.
pub(crate) fn domain_hash(hash: crate::BlockHash) -> zaino_primitives::types::BlockHash {
    zaino_primitives::types::BlockHash::from(hash.0)
}

/// This crate's height, as the domain names it.
///
/// `None` when the height is beyond the protocol maximum. This crate's height
/// is any `u32`, where the domain's is validated, so a caller asking about an
/// impossible height gets the same answer it would for an absent one: nothing
/// is there.
pub(crate) fn domain_height(height: crate::Height) -> Option<zaino_primitives::types::Height> {
    zaino_primitives::types::Height::try_from(height.0).ok()
}

/// A [`ChainHeadBlock`] could not be expressed as an [`IndexedBlock`].
#[derive(Debug, thiserror::Error)]
pub enum ChainHeadConversionError {
    /// The block's fields could not be expressed in the stored form.
    ///
    /// The field mapping itself belongs to the store — `IndexedBlock` is its
    /// shape, and it must be the one place that decides how a block becomes
    /// one. This wraps its rejection rather than restating it, so a new
    /// rejection reason does not need a matching variant here.
    #[error(transparent)]
    Conversion(#[from] zaino_chain_store_zainodb::conversion::BlockConversionError),
}

/// Re-expresses a ChainHead block as an `IndexedBlock`.
///
/// Only the ChainHead-specific part is here: unwrapping the block into the
/// domain pieces the store's conversion takes, and turning ChainHead's
/// anchor-relative work into the type `IndexedBlock` stores. The field mapping
/// itself lives in the store, because `IndexedBlock` is the store's shape.
///
/// That split is deliberate. Neither subsystem may depend on the other, and
/// this does not make one: the store's conversion names nothing from either
/// chain-head crate — it takes a `zaino-primitives` block, its tree roots, and
/// a chainwork. `ChainIndex` depends on both halves already, and this is its
/// adapter. What it avoids is a second copy of the mapping, which is what the
/// codebase had, and which had already drifted over transparent script
/// classification.
pub fn indexed_block(block: &ChainHeadBlock) -> Result<IndexedBlock, ChainHeadConversionError> {
    Ok(zaino_chain_store_zainodb::conversion::indexed_block(
        &block.block,
        &block.tree_roots,
        chainwork(block.work),
    )?)
}

/// ChainHead's anchor-relative work, as the type `IndexedBlock` stores.
///
/// Non-zero by construction: ChainHead starts each accumulation at the anchor
/// block's own work rather than at zero, precisely so this conversion cannot
/// fail.
fn chainwork(work: ChainHeadWork) -> ChainWork {
    ChainWork::try_new(work.as_u128())
        .expect("chain head work is accumulated from a non-zero anchor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_index::tests::vectors::{indexed_block_chain, load_test_vectors};
    use crate::chain_index::types::TxInCompact;
    use zaino_primitives::types::TreeRoots;

    /// This conversion and the finalised state's must produce the same
    /// `IndexedBlock` from the same block.
    ///
    /// The two arrive from opposite directions — this one from a parsed domain
    /// block, the finalised state's from a `zebra_chain` block — and ChainIndex
    /// reads both halves of the chain through the result. A field either side
    /// filled differently would show up as a block that changes shape when it
    /// crosses the finalised seam, which is exactly the class of bug that is
    /// invisible until a reorg moves the boundary.
    ///
    /// It is also what pins the store's own build path. That path now assembles
    /// its blocks from domain blocks where it used to assemble them from
    /// `zebra_chain` ones, and the result is written to disk — so this asserts
    /// the two agree byte-for-byte, against the block vectors whose stored
    /// encodings the golden tests already pin. `indexed_block_chain` is the
    /// oracle: it still builds through the old `zebra_chain` path.
    ///
    /// Anchoring both accumulations at the same first block makes even
    /// chainwork comparable, so this is a total comparison. It became total
    /// when the two paths stopped disagreeing about the coinbase input: the
    /// store's conversion synthesises the null prevout a domain block drops,
    /// because that input is a persisted field.
    #[test]
    fn conversion_agrees_with_the_finalised_state_path() {
        let vectors = load_test_vectors().expect("test vectors load");
        let expected: Vec<IndexedBlock> = indexed_block_chain(&vectors.blocks).collect();

        let mut work: Option<ChainHeadWork> = None;
        for (vector, expected) in vectors.blocks.iter().zip(&expected) {
            let block = zaino_convert_zebra::block_from_zebra(
                &vector.zebra_block,
                zaino_primitives::types::ChainMetadata {
                    sapling_tree_size: vector.sapling_tree_size as u32,
                    orchard_tree_size: vector.orchard_tree_size as u32,
                    ironwood_tree_size: 0,
                },
            )
            .expect("vector block converts to the domain shape");

            let block_work = zaino_consensus::work_from_bits(block.header.bits)
                .expect("vector block has valid difficulty");
            let accumulated = match work {
                Some(parent) => parent.checked_add(block_work).expect("no overflow"),
                None => ChainHeadWork::anchored_at(block_work),
            };
            work = Some(accumulated);

            let chain_head_block = ChainHeadBlock {
                reference: zaino_primitives::types::BlockRef {
                    hash: block.header.hash,
                    height: block.header.height,
                },
                parent_hash: block.header.prev_hash,
                work: accumulated,
                block,
                tree_roots: TreeRoots {
                    sapling: Some(zaino_primitives::types::TreeRootInfo {
                        root: <[u8; 32]>::from(vector.sapling_root).into(),
                        size: vector.sapling_tree_size,
                    }),
                    orchard: Some(zaino_primitives::types::TreeRootInfo {
                        root: <[u8; 32]>::from(vector.orchard_root).into(),
                        size: vector.orchard_tree_size,
                    }),
                    ironwood: None,
                },
            };

            let actual = indexed_block(&chain_head_block).expect("conversion succeeds");

            assert_eq!(actual.context, expected.context, "block context");
            assert_eq!(actual.data, expected.data, "block header data");
            assert_eq!(
                actual.commitment_tree_data, expected.commitment_tree_data,
                "commitment tree data",
            );
            assert_eq!(
                actual.transactions.len(),
                expected.transactions.len(),
                "transaction count",
            );

            for (actual_tx, expected_tx) in actual.transactions.iter().zip(expected.transactions())
            {
                assert_eq!(
                    actual_tx.transparent().inputs(),
                    expected_tx.transparent().inputs(),
                    "transparent inputs",
                );
                assert_eq!(
                    actual_tx.transparent().outputs(),
                    expected_tx.transparent().outputs(),
                    "transparent outputs",
                );
                assert_eq!(actual_tx.index(), expected_tx.index(), "transaction index");
                assert_eq!(actual_tx.txid(), expected_tx.txid(), "txid");
                assert_eq!(
                    actual_tx.balances(),
                    expected_tx.balances(),
                    "pool balances"
                );
            }
        }
    }

    /// The coinbase input is synthesised back, and exactly once.
    ///
    /// A domain block drops the coinbase's null prevout; the stored form keeps
    /// it. Getting this wrong is not cosmetic — the input is a persisted field,
    /// so a missing one changes the bytes of every block on disk and a
    /// duplicated one changes them differently. Asserted on both sides so the
    /// invariant is pinned to the chain rather than to one conversion.
    #[test]
    fn the_coinbase_null_prevout_is_present_exactly_once() {
        let vectors = load_test_vectors().expect("test vectors load");

        for expected in indexed_block_chain(&vectors.blocks) {
            for transaction in expected.transactions() {
                let nulls = transaction
                    .transparent()
                    .inputs()
                    .iter()
                    .filter(|input| **input == TxInCompact::null_prevout())
                    .count();
                let expected_nulls = usize::from(transaction.index() == 0);
                assert_eq!(
                    nulls,
                    expected_nulls,
                    "transaction {} should carry {expected_nulls} null prevout(s)",
                    transaction.index(),
                );
            }
        }
    }
}
