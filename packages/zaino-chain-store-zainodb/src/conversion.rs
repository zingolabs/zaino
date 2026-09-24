//! Building this backend's [`IndexedBlock`] from a domain block.
//!
//! The store fetches blocks through the `zaino-source` ports, which yield
//! [`zaino_primitives::types::Block`]. This module is the one place that turns
//! one into the shape the store writes to disk.
//!
//! # Why this lives here
//!
//! [`IndexedBlock`] is this backend's persisted vocabulary, so this backend
//! owns its construction. The inputs are all `zaino-primitives`: nothing here
//! names a chain-head type, a validator type, or a wire type. That is what lets
//! a consumer that already holds a domain block — ChainIndex's chain-head
//! adapter does, while it still reads both halves of the chain through
//! `IndexedBlock` — reuse this rather than keep a second copy of the field
//! mapping. Two copies is what the codebase had, and they had already drifted
//! over transparent script classification.
//!
//! # Two places where a domain block and a `zebra_chain` block differ
//!
//! This conversion replaced one written against `zebra_chain::block::Block`.
//! The result must be byte-identical, because databases already exist. Two
//! differences are real and are reconciled here rather than left to the caller:
//!
//! 1. **Block commitments.** The old path recomputed the commitment from the
//!    block and the network, where the domain header carries the field as it
//!    was mined. These agree for every block that parses: the recomputation
//!    round-trips the header bytes for every network upgrade, including the
//!    reserved-value case whose only legal value is the all-zero one. So no
//!    network parameter is needed, and the pinned block vectors prove it.
//!
//! 2. **The coinbase input.** A domain block carries only real prevouts —
//!    `zaino-convert-zebra` drops the coinbase's null one. The stored form
//!    keeps it, so it is synthesised back for the transaction at index 0. This
//!    is not cosmetic: it is a persisted field, and dropping it would change
//!    the bytes of every block on disk.

/// The domain compact block and pool filter, as the light-wallet wire carries
/// them.
///
/// Re-exported here because the implementations sit beside the reader that has
/// always produced them, and a second copy at the crate's public edge would be
/// a second thing to keep in step with the protocol. Naming follows the
/// project's wire-boundary rule (`to_wire` / `from_wire`) rather than the
/// `_proto` suffix they carried while they were private.
///
/// # Temporary
///
/// A storage crate has no business building wire messages. These are public
/// because ChainIndex reads compact blocks through
/// [`zaino_chain_store::CompactBlockRead`], which yields domain blocks, and
/// still answers its callers in the wire shape — so the conversion has to be
/// reachable from outside. Both move to the serving side, together with this
/// crate's `zaino-proto` dependency, when ChainIndex's wire surface goes.
pub use crate::store::finalised_source::v1::compact_block::{
    compact_block_to_wire, pool_filter_from_wire,
};

use zaino_primitives::types::{classify_script, Block, Transaction, TreeRoots};

use crate::types::{
    db::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes},
    AbsoluteChainWork, BlockContext, BlockData, BlockHash, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactTxData, EquihashSolution, Height,
    IndexedBlock, OrchardCompactTx, SaplingCompactTx, ScriptType, SingleBlockWork, TransactionHash,
    TransparentCompactTx, TxInCompact, TxOutCompact, GENESIS_HEIGHT,
};

/// A domain block could not be expressed as an [`IndexedBlock`].
#[derive(Debug, thiserror::Error)]
pub enum BlockConversionError {
    /// A transparent output's value exceeds what the compact form can hold.
    #[error("block {hash} has a transparent output that cannot be compacted")]
    OutputNotCompactable {
        /// The block that could not be converted.
        hash: BlockHash,
    },

    /// Accumulating this block's work onto its parent's overflowed.
    #[error("chainwork overflow at block {hash}: {reason}")]
    ChainWorkOverflow {
        /// The block whose work could not be accumulated.
        hash: BlockHash,
        /// Why the accumulation failed.
        reason: String,
    },

    /// A transaction's position in the block does not fit the stored index
    /// width.
    ///
    /// The block-order position is a `usize`; the stored compact form records
    /// it as `u64`. Rejected rather than truncated: a wrapped position would
    /// put a wrong index on disk. This
    /// cannot happen for any real block — the block size limit bounds the
    /// transaction count far below `u64::MAX` — but the conversion refuses it
    /// rather than assert it away.
    #[error("block {hash} has a transaction position that does not fit into u64: {position}")]
    TxPositionOverflow {
        /// The block that could not be converted.
        hash: BlockHash,
        /// The position that did not fit.
        position: usize,
    },
    /// A block above genesis was built without its parent's chainwork, which only genesis may lack.
    #[error("block {hash} at height {height} has no parent chainwork to accumulate onto")]
    ParentChainWorkUnknown {
        /// The block that could not be converted.
        hash: BlockHash,
        /// The block's height, which is above genesis.
        height: Height,
    },
}

/// This block's chainwork accumulated onto its parent's, which is the block's own work at [`GENESIS_HEIGHT`] and an error above it when the parent's chainwork is unknown.
pub fn chainwork_from_parent(
    block_work: SingleBlockWork,
    hash: BlockHash,
    height: Height,
    parent_chainwork: Option<AbsoluteChainWork>,
) -> Result<AbsoluteChainWork, BlockConversionError> {
    match parent_chainwork {
        Some(parent) => {
            parent
                .accumulate(block_work)
                .map_err(|error| BlockConversionError::ChainWorkOverflow {
                    hash,
                    reason: error.to_string(),
                })
        }
        None if height == GENESIS_HEIGHT => Ok(AbsoluteChainWork::genesis(block_work)),
        None => Err(BlockConversionError::ParentChainWorkUnknown { hash, height }),
    }
}

/// [`chainwork_from_parent`] for a builder that may not know the parent's chainwork, whose block above genesis then has none.
pub fn chainwork_from_parent_if_known(
    block_work: SingleBlockWork,
    hash: BlockHash,
    height: Height,
    parent_chainwork: Option<AbsoluteChainWork>,
) -> Result<Option<AbsoluteChainWork>, BlockConversionError> {
    match chainwork_from_parent(block_work, hash, height, parent_chainwork) {
        Err(BlockConversionError::ParentChainWorkUnknown { .. }) => Ok(None),
        result => result.map(Some),
    }
}

/// Re-expresses a domain block as this backend's [`IndexedBlock`], taking the cumulative `tree_roots` and the `chainwork` that a block alone does not determine in whichever form the caller holds.
pub fn indexed_block<Work>(
    block: &Block,
    tree_roots: &TreeRoots,
    chainwork: Work,
) -> Result<IndexedBlock<Work>, BlockConversionError> {
    let hash = BlockHash(block.header.hash.into());

    let data = block_data(&block.header);

    let transactions = block
        .transactions()
        .iter()
        .enumerate()
        .map(|(position, transaction)| compact_transaction(position, transaction, hash))
        .collect::<Result<Vec<_>, _>>()?;

    let context = BlockContext::new(
        hash,
        BlockHash(block.header.prev_hash.into()),
        chainwork,
        Height(u32::from(block.header.height)),
    );

    Ok(IndexedBlock::new(
        context,
        data,
        transactions,
        commitment_tree_data(tree_roots),
    ))
}

/// A block header's own fields, as this backend's [`BlockData`].
///
/// Shared with the read direction rather than restated there. Both directions
/// start from the same [`BlockHeader`] — a block arriving from a validator and
/// a block read back off disk carry the identical type — so a second copy of
/// this mapping is not a parallel implementation but the same one, free to
/// drift. Total: every fallible field, difficulty included, is already
/// validated by the types the header carries.
///
/// `pub(crate)` for the sibling adapter, which is the only other caller.
pub(crate) fn block_data(header: &zaino_primitives::types::BlockHeader) -> BlockData {
    BlockData {
        version: header.version,
        time: i64::from(header.time),
        merkle_root: header.merkle_root.into(),
        block_commitments: header.block_commitments.into(),
        bits: header.bits,
        nonce: header.nonce,
        solution: solution(&header.solution),
    }
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

/// The stored treestate for a block, where an absent sapling or orchard root stores as a zero root with size zero and an absent ironwood root stores as `None`.
pub fn commitment_tree_data(roots: &TreeRoots) -> CommitmentTreeData {
    let root_bytes = |root: &Option<zaino_primitives::types::TreeRootInfo>| {
        root.as_ref().map(|info| <[u8; 32]>::from(info.root))
    };
    let size = |root: &Option<zaino_primitives::types::TreeRootInfo>| {
        root.as_ref().map_or(0, |info| u32::from(info.size))
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

/// `position` is the transaction's slot in block order, the sole authority for
/// both its served index and its coinbase-ness. It is threaded in from the
/// caller's `enumerate` rather than read off the transaction, which no longer
/// stores it.
fn compact_transaction(
    position: usize,
    transaction: &Transaction,
    block: BlockHash,
) -> Result<CompactTxData, BlockConversionError> {
    let index = u64::try_from(position).map_err(|_| BlockConversionError::TxPositionOverflow {
        hash: block,
        position,
    })?;
    Ok(CompactTxData::new(
        index,
        TransactionHash(transaction.txid.into()),
        transparent(position, transaction, block)?,
        sapling(transaction),
        orchard_shaped(&transaction.orchard),
        orchard_shaped(&transaction.ironwood),
    ))
}

/// The transparent inputs and outputs, in stored compact form.
///
/// The transaction at position 0 — the coinbase — gets its null prevout back;
/// see this module's header. Coinbase-ness is decided by block-order position,
/// not by any field on the transaction. Every other transaction's inputs are
/// already complete.
fn transparent(
    position: usize,
    transaction: &Transaction,
    block: BlockHash,
) -> Result<TransparentCompactTx, BlockConversionError> {
    let is_coinbase = position == 0;

    let mut inputs: Vec<TxInCompact> =
        Vec::with_capacity(transaction.transparent.inputs.len() + usize::from(is_coinbase));
    if is_coinbase {
        inputs.push(TxInCompact::null_prevout());
    }
    inputs.extend(
        transaction
            .transparent
            .inputs
            .iter()
            .map(|input| TxInCompact::new(input.prev_txid.into(), input.prev_index)),
    );

    let outputs = transaction
        .transparent
        .outputs
        .iter()
        .map(|output| {
            let script: Vec<u8> = output.script.clone().into();
            let (hash, script_type) = classify_script(&script);

            TxOutCompact::new(u64::from(output.value), hash, script_tag(script_type))
                .ok_or(BlockConversionError::OutputNotCompactable { hash: block })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TransparentCompactTx::new(inputs, outputs))
}

/// This backend's on-disk tag for a [`classify_script`] result.
///
/// The classification is shared vocabulary; the byte written for it is a
/// storage detail, so the mapping lives here rather than on the shared type.
fn script_tag(script_type: zaino_primitives::types::ScriptType) -> u8 {
    match script_type {
        zaino_primitives::types::ScriptType::P2PKH => ScriptType::P2PKH as u8,
        zaino_primitives::types::ScriptType::P2SH => ScriptType::P2SH as u8,
        zaino_primitives::types::ScriptType::NonStandard => ScriptType::NonStandard as u8,
    }
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
                    output.enc_ciphertext.into(),
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
                    action.enc_ciphertext.into(),
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod chainwork_from_parent {
    use super::*;
    use crate::types::CompactDifficulty;

    fn work() -> SingleBlockWork {
        CompactDifficulty::try_from_bits(0x2007_ffff)
            .expect("a valid nBits")
            .to_work()
    }

    fn hash() -> BlockHash {
        BlockHash([1u8; 32])
    }

    /// A block above genesis whose parent's chainwork is unknown cannot be given one, and is not given genesis work.
    #[test]
    fn an_unknown_parent_above_genesis_is_an_error() {
        let error = chainwork_from_parent(work(), hash(), Height(1), None)
            .expect_err("no parent to accumulate onto");
        assert!(matches!(
            error,
            BlockConversionError::ParentChainWorkUnknown {
                height: Height(1),
                ..
            }
        ));
    }

    /// Genesis has no parent, and its chainwork is its own work.
    #[test]
    fn genesis_chainwork_is_its_own_work() {
        let chainwork = chainwork_from_parent(work(), hash(), GENESIS_HEIGHT, None)
            .expect("nothing to overflow");
        assert!(chainwork == AbsoluteChainWork::genesis(work()));
    }

    /// A block with a known parent accumulates its own work onto the parent's.
    #[test]
    fn a_known_parent_accumulates() {
        let parent = AbsoluteChainWork::genesis(work());
        let chainwork = chainwork_from_parent(work(), hash(), Height(1), Some(parent))
            .expect("nothing to overflow");
        assert!(chainwork == parent.accumulate(work()).expect("no overflow"));
    }

    /// A block above genesis whose parent's chainwork is unknown has no chainwork, not genesis work.
    #[test]
    fn if_known_leaves_an_unknown_parent_above_genesis_without_chainwork() {
        let chainwork = chainwork_from_parent_if_known(work(), hash(), Height(1), None)
            .expect("nothing to overflow");
        assert!(chainwork.is_none());
    }

    /// The optional form still seeds genesis and still accumulates onto a known parent.
    #[test]
    fn if_known_agrees_with_the_total_form_where_that_form_answers() {
        let genesis = chainwork_from_parent_if_known(work(), hash(), GENESIS_HEIGHT, None)
            .expect("nothing to overflow");
        assert!(genesis == Some(AbsoluteChainWork::genesis(work())));

        let parent = AbsoluteChainWork::genesis(work());
        let next = chainwork_from_parent_if_known(work(), hash(), Height(1), Some(parent))
            .expect("nothing to overflow");
        assert!(next == Some(parent.accumulate(work()).expect("no overflow")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{TransactionId, TransparentData, TransparentInput};

    fn tx_with_one_real_input() -> Transaction {
        Transaction {
            txid: TransactionId::from([7u8; 32]),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: TransactionId::from([9u8; 32]),
                    prev_index: 3,
                }],
                outputs: Vec::new(),
            },
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    /// The coinbase's synthesised null prevout keys on block-order position,
    /// nothing on the transaction. The *same* transaction gets the null prevout
    /// prepended at position 0 and does not at any other position — proof that
    /// position is the sole coinbase authority now that the transaction stores
    /// no index that could disagree.
    #[test]
    fn null_prevout_is_synthesised_by_position_not_by_a_field() {
        let hash = BlockHash([0u8; 32]);
        let transaction = tx_with_one_real_input();

        let at_zero = transparent(0, &transaction, hash).expect("a compactable tx");
        assert!(
            at_zero.inputs()[0].is_null_prevout(),
            "position 0 is the coinbase, so it gets the null prevout"
        );
        assert_eq!(
            at_zero.inputs().len(),
            2,
            "null prevout precedes the one real input"
        );
        assert!(!at_zero.inputs()[1].is_null_prevout());

        let at_one = transparent(1, &transaction, hash).expect("a compactable tx");
        assert_eq!(
            at_one.inputs().len(),
            1,
            "a non-coinbase keeps only its real inputs"
        );
        assert!(!at_one.inputs()[0].is_null_prevout());
    }

    /// The served compact index is the block-order position handed in, so a
    /// block converted transaction-by-transaction reports each transaction's
    /// slot as its index.
    #[test]
    fn served_index_is_the_position() {
        let hash = BlockHash([0u8; 32]);
        let transaction = tx_with_one_real_input();

        for (position, expected) in [(0usize, 0u64), (1, 1), (42, 42)] {
            let compact =
                compact_transaction(position, &transaction, hash).expect("a compactable tx");
            assert_eq!(compact.index(), expected);
        }
    }
}
