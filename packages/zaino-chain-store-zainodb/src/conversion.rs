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
    BlockContext, BlockData, BlockHash, BlockWork, ChainWork, CompactDifficulty,
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTxData,
    EquihashSolution, Height, IndexedBlock, OrchardCompactTx, SaplingCompactTx, ScriptType,
    TransactionHash, TransparentCompactTx, TxInCompact, TxOutCompact,
};

/// A domain block could not be expressed as an [`IndexedBlock`].
#[derive(Debug, thiserror::Error)]
pub enum BlockConversionError {
    /// The header's difficulty does not decode to a valid target.
    #[error("block {hash} has invalid difficulty: {reason}")]
    InvalidDifficulty {
        /// The block that could not be converted.
        hash: BlockHash,
        /// Why the difficulty was rejected.
        reason: String,
    },

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

    /// A commitment tree has grown past what the stored form can record.
    ///
    /// The domain counts tree sizes in `u64` where the stored form uses `u32`.
    /// Rejected rather than truncated: a silently wrapped size would put a
    /// wrong treestate on disk, which no later read could detect.
    #[error("block {hash} has a {pool} commitment tree size that does not fit into u32: {size}")]
    TreeSizeOverflow {
        /// The block that could not be converted.
        hash: BlockHash,
        /// Which pool's tree overflowed.
        pool: &'static str,
        /// The size that did not fit.
        size: u64,
    },
}

/// This block's own proof-of-work contribution, ignoring its ancestry.
///
/// Split from [`chainwork_from_parent`] because a bulk sync folds the
/// cumulative work over a run of already-fetched blocks *before* assembling any
/// of them: the fold is the only ordering constraint in block building, and it
/// is pure integer arithmetic, so it must not be held behind the expensive
/// conversion.
pub fn block_work(
    header_bits: zaino_primitives::types::CompactDifficulty,
    hash: BlockHash,
) -> Result<BlockWork, BlockConversionError> {
    Ok(difficulty(header_bits, hash)?.to_work())
}

/// This block's chainwork, accumulated onto its parent's.
///
/// Separate from [`indexed_block`] because the two callers arrive with
/// different work: the store builds forward from its own tip and so has a
/// parent's absolute chainwork, while a caller replaying an in-memory window
/// already holds an accumulated value and passes it straight through.
///
/// `None` for the parent means genesis, whose chainwork is its own work.
pub fn chainwork_from_parent(
    header_bits: zaino_primitives::types::CompactDifficulty,
    hash: BlockHash,
    parent_chainwork: Option<ChainWork>,
) -> Result<ChainWork, BlockConversionError> {
    let block_work = block_work(header_bits, hash)?;
    match parent_chainwork {
        Some(parent) => {
            parent
                .accumulate(block_work)
                .map_err(|error| BlockConversionError::ChainWorkOverflow {
                    hash,
                    reason: error.to_string(),
                })
        }
        None => Ok(ChainWork::genesis(block_work)),
    }
}

/// Re-expresses a domain block as this backend's [`IndexedBlock`].
///
/// `tree_roots` are not taken from `block.chain_metadata`: that carries the
/// pool *sizes* but not the roots, and the stored form needs both. The caller
/// asks its source for them — they are cumulative over the chain and so are not
/// derivable from one block.
///
/// `chainwork` is passed in rather than derived, because a block alone does not
/// determine it. See [`chainwork_from_parent`].
pub fn indexed_block(
    block: &Block,
    tree_roots: &TreeRoots,
    chainwork: ChainWork,
) -> Result<IndexedBlock, BlockConversionError> {
    let hash = BlockHash(block.header.hash.into());

    let data = block_data(&block.header)?;

    let transactions = block
        .transactions
        .iter()
        .map(|transaction| compact_transaction(transaction, hash))
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
        commitment_tree_data(tree_roots, hash)?,
    ))
}

/// A block header's own fields, as this backend's [`BlockData`].
///
/// Shared with the read direction rather than restated there. Both directions
/// start from the same [`BlockHeader`] — a block arriving from a validator and
/// a block read back off disk carry the identical type — so a second copy of
/// this mapping is not a parallel implementation but the same one, free to
/// drift. It already had: the read path stringified the difficulty failure this
/// one keeps typed.
///
/// `pub(crate)` for the sibling adapter, which is the only other caller.
pub(crate) fn block_data(
    header: &zaino_primitives::types::BlockHeader,
) -> Result<BlockData, BlockConversionError> {
    Ok(BlockData {
        version: header.version,
        time: i64::from(header.time),
        merkle_root: header.merkle_root.into(),
        block_commitments: header.block_commitments.into(),
        bits: difficulty(header.bits, BlockHash(header.hash.into()))?,
        nonce: header.nonce,
        solution: solution(&header.solution),
    })
}

fn difficulty(
    bits: zaino_primitives::types::CompactDifficulty,
    hash: BlockHash,
) -> Result<CompactDifficulty, BlockConversionError> {
    CompactDifficulty::try_from_bits(bits).map_err(|error| {
        BlockConversionError::InvalidDifficulty {
            hash,
            reason: error.to_string(),
        }
    })
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

/// The stored treestate for a block.
///
/// An absent sapling or orchard root stores as the all-zero root with size
/// zero, where an absent ironwood root stores as `None`. That asymmetry is on
/// disk already — it is what the v1.2.1 to v1.3.0 migration writes for
/// pre-activation heights — so it is preserved rather than tidied.
///
/// Public so the port layer converts a treestate through this rather than
/// through a second copy of the mapping. The tree-size narrowing below is why
/// that matters: it refuses a size the stored width cannot hold, where a cast
/// would write a smaller one and nothing downstream would notice.
pub fn commitment_tree_data(
    roots: &TreeRoots,
    hash: BlockHash,
) -> Result<CommitmentTreeData, BlockConversionError> {
    let root_bytes = |root: &Option<zaino_primitives::types::TreeRootInfo>| {
        root.as_ref().map(|info| <[u8; 32]>::from(info.root))
    };
    let size = |root: &Option<zaino_primitives::types::TreeRootInfo>,
                pool: &'static str|
     -> Result<u32, BlockConversionError> {
        match root.as_ref() {
            Some(info) => {
                u32::try_from(info.size).map_err(|_| BlockConversionError::TreeSizeOverflow {
                    hash,
                    pool,
                    size: info.size,
                })
            }
            None => Ok(0),
        }
    };

    Ok(CommitmentTreeData::new(
        CommitmentTreeRoots::new(
            root_bytes(&roots.sapling).unwrap_or_default(),
            root_bytes(&roots.orchard).unwrap_or_default(),
            root_bytes(&roots.ironwood),
        ),
        CommitmentTreeSizes::new(
            size(&roots.sapling, "sapling")?,
            size(&roots.orchard, "orchard")?,
            size(&roots.ironwood, "ironwood")?,
        ),
    ))
}

fn compact_transaction(
    transaction: &Transaction,
    block: BlockHash,
) -> Result<CompactTxData, BlockConversionError> {
    Ok(CompactTxData::new(
        u64::from(transaction.index),
        TransactionHash(transaction.txid.into()),
        transparent(transaction, block)?,
        sapling(transaction),
        orchard_shaped(&transaction.orchard),
        orchard_shaped(&transaction.ironwood),
    ))
}

/// The transparent inputs and outputs, in stored compact form.
///
/// The transaction at index 0 gets its null prevout back — see this module's
/// header. Every other transaction's inputs are already complete.
fn transparent(
    transaction: &Transaction,
    block: BlockHash,
) -> Result<TransparentCompactTx, BlockConversionError> {
    let is_coinbase = u64::from(transaction.index) == 0;

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
/// value is zero-padded rather than rejected: the stored form is a fixed-width
/// field, and a source that supplied less has produced a block no wallet can
/// scan regardless.
fn ciphertext_prefix(ciphertext: &zaino_primitives::types::EncryptedCiphertext) -> [u8; 52] {
    let bytes: Vec<u8> = ciphertext.clone().into();
    let mut prefix = [0u8; 52];
    let usable = bytes.len().min(52);
    prefix[..usable].copy_from_slice(&bytes[..usable]);
    prefix
}
