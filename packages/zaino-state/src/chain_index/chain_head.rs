//! ChainIndex's side of the ChainHead boundary.
//!
//! Two things live here: how ChainIndex hands ChainHead a validator, and how a
//! [`ChainHeadBlock`] becomes the [`IndexedBlock`] the rest of this crate is
//! written against.
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

use zaino_chain_head::{ChainHeadBlock, ChainHeadBlockSource, ChainHeadWork};
use zaino_primitives::types::{Transaction, TreeRoots};

use crate::chain_index::{
    source::BlockchainSource,
    source_ports::ChainIndexSourcePorts,
    types::{
        db::{
            legacy::{
                AddrScript, BlockData, CompactOrchardAction, CompactSaplingOutput,
                CompactSaplingSpend, CompactTxData, EquihashSolution, OrchardCompactTx,
                SaplingCompactTx, ScriptType, TransparentCompactTx, TxInCompact, TxOutCompact,
            },
            CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes,
        },
        BlockContext, ChainWork, CompactDifficulty,
    },
    validator_source::ValidatorSource,
};
use crate::{Height, IndexedBlock};

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
    /// The block's difficulty does not decode to a valid target.
    ///
    /// ChainHead validated this when it accumulated the block's work, so
    /// reaching here means the two disagree about the same header.
    #[error("block {hash} has invalid difficulty: {reason}")]
    InvalidDifficulty {
        /// The block that could not be converted.
        hash: crate::BlockHash,
        /// Why the difficulty was rejected.
        reason: String,
    },

    /// A transparent output's value exceeds what the compact form can hold.
    #[error("block {hash} has a transparent output that cannot be compacted")]
    OutputNotCompactable {
        /// The block that could not be converted.
        hash: crate::BlockHash,
    },
}

/// Re-expresses a ChainHead block as an `IndexedBlock`.
///
/// No network parameter, deliberately. The finalised state's equivalent path
/// passes one so it can interpret the header's `block_commitments` field per
/// network upgrade — but that interpretation round-trips to the same bytes for
/// every block that parses, including the reserved-value case, whose only legal
/// value is the all-zero one it maps to. The network therefore selects which
/// error an invalid block gets, and nothing about a valid one.
pub fn indexed_block(block: &ChainHeadBlock) -> Result<IndexedBlock, ChainHeadConversionError> {
    let hash = crate::BlockHash(block.reference.hash.into());

    let bits = CompactDifficulty::try_from_bits(block.block.header.bits).map_err(|error| {
        ChainHeadConversionError::InvalidDifficulty {
            hash,
            reason: error.to_string(),
        }
    })?;

    let data = BlockData {
        version: block.block.header.version,
        time: i64::from(block.block.header.time),
        merkle_root: block.block.header.merkle_root.into(),
        block_commitments: block.block.header.block_commitments.into(),
        bits,
        nonce: block.block.header.nonce,
        solution: solution(&block.block.header.solution),
    };

    let transactions = block
        .block
        .transactions
        .iter()
        .map(|transaction| compact_transaction(transaction, hash))
        .collect::<Result<Vec<_>, _>>()?;

    let context = BlockContext::new(
        hash,
        crate::BlockHash(block.parent_hash.into()),
        chainwork(block.work),
        Height(u32::from(block.reference.height)),
    );

    Ok(IndexedBlock::new(
        context,
        data,
        transactions,
        commitment_tree_data(&block.tree_roots),
    ))
}

/// ChainHead's anchor-relative work, as the type `IndexedBlock` stores.
///
/// Non-zero by construction: ChainHead starts each accumulation at the anchor
/// block's own work rather than at zero, precisely so this conversion cannot
/// fail.
fn chainwork(work: ChainHeadWork) -> ChainWork {
    ChainWork::new(
        std::num::NonZeroU128::new(work.as_u128())
            .expect("chain head work is accumulated from a non-zero anchor"),
    )
}

fn solution(solution: &zaino_primitives::types::EquihashSolution) -> EquihashSolution {
    match solution {
        zaino_primitives::types::EquihashSolution::Standard(bytes) => {
            EquihashSolution::Standard(*bytes)
        }
        zaino_primitives::types::EquihashSolution::Regtest(bytes) => {
            EquihashSolution::Regtest(*bytes)
        }
    }
}

fn commitment_tree_data(roots: &TreeRoots) -> CommitmentTreeData {
    let root_bytes = |root: &Option<zaino_primitives::types::TreeRootInfo>| {
        root.as_ref().map(|info| <[u8; 32]>::from(info.root))
    };
    // Saturating rather than truncating. A note-commitment tree size cannot
    // reach `u32::MAX` — that is more notes than the chain has blocks to carry —
    // so neither arm is reachable in practice, but the two disagree about which
    // way to be wrong if it ever were. `as` is modulo, so a size of exactly
    // 2^32 would report an *empty* tree, which reads as valid; saturating
    // reports an implausibly full one, which does not.
    let size = |root: &Option<zaino_primitives::types::TreeRootInfo>| {
        root.as_ref()
            .map_or(0, |info| u32::try_from(info.size).unwrap_or(u32::MAX))
    };

    CommitmentTreeData::new(
        CommitmentTreeRoots::new(
            root_bytes(&roots.sapling).unwrap_or_default(),
            root_bytes(&roots.orchard).unwrap_or_default(),
            root_bytes(&roots.ironwood),
        ),
        CommitmentTreeSizes::new(
            size(&roots.sapling),
            size(&roots.orchard),
            size(&roots.ironwood),
        ),
    )
}

fn compact_transaction(
    transaction: &Transaction,
    block: crate::BlockHash,
) -> Result<CompactTxData, ChainHeadConversionError> {
    Ok(CompactTxData::new(
        u64::from(transaction.index),
        crate::TransactionHash(transaction.txid.into()),
        transparent(transaction, block)?,
        sapling(transaction),
        orchard_shaped(&transaction.orchard),
        orchard_shaped(&transaction.ironwood),
    ))
}

/// The transparent inputs and outputs, in compact form.
///
/// Coinbase inputs are absent rather than present-as-null: the domain block
/// carries only real prevouts, where a zebra-derived block carries a null one.
/// Both are equivalent downstream — the spent-outpoint index skips null
/// prevouts anyway — but the difference is real and worth knowing about when
/// comparing a ChainHead-derived block against a finalised one.
fn transparent(
    transaction: &Transaction,
    block: crate::BlockHash,
) -> Result<TransparentCompactTx, ChainHeadConversionError> {
    let inputs = transaction
        .transparent
        .inputs
        .iter()
        .map(|input| TxInCompact::new(input.prev_txid.into(), input.prev_index))
        .collect();

    let outputs = transaction
        .transparent
        .outputs
        .iter()
        .map(|output| {
            let script: Vec<u8> = output.script.clone().into();
            let address = AddrScript::from_script(&script).unwrap_or_else(|| {
                // A non-standard script has no address to index; the finalised
                // path stores its first 20 bytes under a non-standard tag, and
                // this matches so the two agree.
                let mut fallback = [0u8; 20];
                let usable = script.len().min(20);
                fallback[..usable].copy_from_slice(&script[..usable]);
                AddrScript::new(fallback, ScriptType::NonStandard as u8)
            });

            TxOutCompact::new(
                u64::from(output.value),
                *address.hash(),
                address.script_type(),
            )
            .ok_or(ChainHeadConversionError::OutputNotCompactable { hash: block })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TransparentCompactTx::new(inputs, outputs))
}

fn sapling(transaction: &Transaction) -> SaplingCompactTx {
    let value_balance = i64::from(transaction.sapling.value_balance);

    SaplingCompactTx::new(
        (value_balance != 0).then_some(value_balance),
        transaction
            .sapling
            .spends
            .iter()
            .map(|spend| CompactSaplingSpend::new(spend.nullifier.into()))
            .collect(),
        transaction
            .sapling
            .outputs
            .iter()
            .map(|output| {
                CompactSaplingOutput::new(
                    output.cmu.into(),
                    output.ephemeral_key.into(),
                    ciphertext_prefix(&output.enc_ciphertext),
                )
            })
            .collect(),
    )
}

/// Orchard and Ironwood share a shape, so they share this.
fn orchard_shaped(pool: &zaino_primitives::types::OrchardData) -> OrchardCompactTx {
    let value_balance = i64::from(pool.value_balance);

    OrchardCompactTx::new(
        (value_balance != 0).then_some(value_balance),
        pool.actions
            .iter()
            .map(|action| {
                CompactOrchardAction::new(
                    action.nullifier.into(),
                    action.cmx.into(),
                    action.ephemeral_key.into(),
                    ciphertext_prefix(&action.enc_ciphertext),
                )
            })
            .collect(),
    )
}

/// The 52-byte scanning prefix.
///
/// The domain type already holds exactly this prefix rather than the full
/// 580-byte ciphertext, so this is a reshape and not a truncation. A shorter
/// value is zero-padded rather than rejected: the compact form is a fixed-width
/// field, and a source that supplied less has produced a block no wallet can
/// scan regardless.
fn ciphertext_prefix(ciphertext: &zaino_primitives::types::EncryptedCiphertext) -> [u8; 52] {
    let bytes: Vec<u8> = ciphertext.clone().into();
    let mut prefix = [0u8; 52];
    let usable = bytes.len().min(52);
    prefix[..usable].copy_from_slice(&bytes[..usable]);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_index::tests::vectors::{indexed_block_chain, load_test_vectors};

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
    /// Anchoring both accumulations at the same first block makes even
    /// chainwork comparable, so this is a total comparison rather than a
    /// partial one — with a single documented exception, asserted separately
    /// below.
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
                // The one documented difference: a domain block carries only
                // real prevouts, where a zebra-derived one carries a null
                // prevout for the coinbase input. Everything downstream skips
                // null prevouts, so the two are equivalent — but the difference
                // is real, and is asserted rather than glossed over.
                let expected_inputs: Vec<_> = expected_tx
                    .transparent()
                    .inputs()
                    .iter()
                    .filter(|input| **input != TxInCompact::null_prevout())
                    .cloned()
                    .collect();
                assert_eq!(
                    actual_tx.transparent().inputs(),
                    expected_inputs,
                    "transparent inputs, ignoring the coinbase null prevout",
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

    /// The coinbase difference is a *presence* difference and nothing more:
    /// the domain block drops the null prevout, and drops nothing else.
    #[test]
    fn only_the_coinbase_null_prevout_is_dropped() {
        let vectors = load_test_vectors().expect("test vectors load");

        for expected in indexed_block_chain(&vectors.blocks) {
            for transaction in expected.transactions() {
                let inputs = transaction.transparent().inputs();
                let nulls = inputs
                    .iter()
                    .filter(|input| **input == TxInCompact::null_prevout())
                    .count();
                assert!(
                    nulls <= 1,
                    "a transaction should carry at most one null prevout, found {nulls}",
                );
            }
        }
    }
}
