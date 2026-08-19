//! Building the indexed on-disk shapes from an independently-parsed block.
//!
//! These were `TryFrom` impls on `IndexedBlock` and `CompactTxData` inside
//! `zaino-state`, reachable only from the `e2e` test-vector generator. They are
//! free functions here for two reasons: the orphan rule forbids implementing a
//! foreign trait for a foreign type from this crate, and — more to the point —
//! logic that exists to *generate test vectors* belongs with the vectors, not in
//! a production crate.
//!
//! Nothing in production builds an `IndexedBlock` this way. The sync path builds
//! one from `BlockWithMetadata`, and that impl is untouched.

use zaino_state::chain_index::types::{
    parse_standard_script, BlockContext, BlockData, ChainWork, CommitmentTreeData,
    CommitmentTreeRoots, CommitmentTreeSizes, CompactDifficulty, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactTxData, EquihashSolution, IndexedBlock,
    OrchardCompactTx, SaplingCompactTx, ScriptType, TransparentCompactTx, TxInCompact,
    TxOutCompact,
};
use zaino_state::{BlockHash, Height};

use super::{block::FullBlock, transaction::FullTransaction};

/// Converts one Orchard-shaped action tuple into its compact on-disk form.
///
/// Shared by the Orchard and Ironwood pools, which have the same action shape;
/// `pool` names the pool only so a length failure says which one.
fn compact_orchard_action_from_parts(
    pool: &str,
    (nullifier, cmx, ephemeral_key, ciphertext): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
) -> Result<CompactOrchardAction, String> {
    let nf: [u8; 32] = nullifier
        .try_into()
        .map_err(|_| format!("{pool} nullifier must be 32 bytes"))?;
    let cmx: [u8; 32] = cmx
        .try_into()
        .map_err(|_| format!("{pool} cmx must be 32 bytes"))?;
    let epk: [u8; 32] = ephemeral_key
        .try_into()
        .map_err(|_| format!("{pool} ephemeral_key must be 32 bytes"))?;
    let ct: [u8; 52] = ciphertext
        .get(..52)
        .ok_or_else(|| format!("{pool} ciphertext must be at least 52 bytes"))?
        .try_into()
        .map_err(|_| format!("{pool} ciphertext must be 52 bytes"))?;
    Ok(CompactOrchardAction::new(nf, cmx, epk, ct))
}

/// Builds the indexed compact transaction from an independently-parsed one.
///
/// `index` is the transaction's position within its block.
pub fn compact_tx_data_from_full_transaction(
    index: u64,
    tx: FullTransaction,
) -> Result<CompactTxData, String> {
    let txid: [u8; 32] = tx
        .tx_id()
        .try_into()
        .map_err(|_| "txid must be 32 bytes".to_string())?;

    let (sapling_balance, orchard_balance, ironwood_balance) = tx.value_balances();

    let vin: Vec<TxInCompact> = tx
        .transparent_inputs()
        .into_iter()
        .map(|(prev_txid, prev_index, _)| {
            let prev_txid_arr: [u8; 32] = prev_txid
                .try_into()
                .map_err(|_| "prev_txid must be 32 bytes".to_string())?;
            Ok::<_, String>(TxInCompact::new(prev_txid_arr, prev_index))
        })
        .collect::<Result<_, _>>()?;

    // A script this parser does not recognise is stored as its first 20 bytes
    // under `NonStandard`, matching how the indexer stores one.
    let vout: Vec<TxOutCompact> = tx
        .transparent_outputs()
        .into_iter()
        .filter_map(|(value, script)| {
            if let Some((hash20, stype)) = parse_standard_script(&script) {
                TxOutCompact::new(value, hash20, stype as u8)
            } else {
                let mut fallback = [0u8; 20];
                let copy_len = script.len().min(20);
                fallback[..copy_len].copy_from_slice(&script[..copy_len]);
                TxOutCompact::new(value, fallback, ScriptType::NonStandard as u8)
            }
        })
        .collect();

    let transparent = TransparentCompactTx::new(vin, vout);

    let spends: Vec<CompactSaplingSpend> = tx
        .shielded_spends()
        .into_iter()
        .map(|nf| {
            let arr: [u8; 32] = nf
                .try_into()
                .map_err(|_| "sapling nullifier must be 32 bytes".to_string())?;
            Ok::<_, String>(CompactSaplingSpend::new(arr))
        })
        .collect::<Result<_, _>>()?;

    let outputs: Vec<CompactSaplingOutput> = tx
        .shielded_outputs()
        .into_iter()
        .map(|(cmu, epk, ct)| {
            let cmu: [u8; 32] = cmu
                .try_into()
                .map_err(|_| "cmu must be 32 bytes".to_string())?;
            let epk: [u8; 32] = epk
                .try_into()
                .map_err(|_| "ephemeral_key must be 32 bytes".to_string())?;
            let ct: [u8; 52] = ct
                .get(..52)
                .ok_or("ciphertext must be at least 52 bytes")?
                .try_into()
                .map_err(|_| "ciphertext must be 52 bytes".to_string())?;
            Ok::<_, String>(CompactSaplingOutput::new(cmu, epk, ct))
        })
        .collect::<Result<_, _>>()?;

    let sapling = SaplingCompactTx::new(sapling_balance, spends, outputs);

    let orchard_actions: Vec<CompactOrchardAction> = tx
        .orchard_actions()
        .into_iter()
        .map(|action| compact_orchard_action_from_parts("orchard", action))
        .collect::<Result<_, _>>()?;
    let orchard = OrchardCompactTx::new(orchard_balance, orchard_actions);

    let ironwood_actions: Vec<CompactOrchardAction> = tx
        .ironwood_actions()
        .into_iter()
        .map(|action| compact_orchard_action_from_parts("ironwood", action))
        .collect::<Result<_, _>>()?;
    let ironwood = OrchardCompactTx::new(ironwood_balance, ironwood_actions);

    Ok(CompactTxData::new(
        index,
        txid.into(),
        transparent,
        sapling,
        orchard,
        ironwood,
    ))
}

/// Builds an indexed block from an independently-parsed one.
///
/// The commitment tree roots and the parent's tree sizes and chainwork are not
/// in the block, so the caller supplies them — the same facts the sync path
/// carries in its `BlockWithMetadata`.
#[allow(clippy::too_many_arguments)]
pub fn indexed_block_from_full_block(
    full_block: FullBlock,
    parent_chainwork: Option<ChainWork>,
    final_sapling_root: [u8; 32],
    final_orchard_root: [u8; 32],
    final_ironwood_root: Option<[u8; 32]>,
    parent_sapling_size: u32,
    parent_orchard_size: u32,
    parent_ironwood_size: u32,
) -> Result<IndexedBlock, String> {
    let header = full_block.header();
    let height = Height::try_from(full_block.height() as u32)
        .map_err(|e| format!("Invalid block height: {e}"))?;

    let hash: [u8; 32] = header
        .cached_hash()
        .try_into()
        .map_err(|_| "Block hash must be 32 bytes")?;
    let parent_hash: [u8; 32] = header
        .hash_prev_block()
        .try_into()
        .map_err(|_| "Parent block hash must be 32 bytes")?;

    let merkle_root: [u8; 32] = header
        .hash_merkle_root()
        .try_into()
        .map_err(|v: Vec<u8>| format!("merkle root must be 32 bytes, got {}", v.len()))?;

    let block_commitments: [u8; 32] = header
        .final_sapling_root()
        .try_into()
        .map_err(|v: Vec<u8>| format!("block commitment must be 32 bytes, got {}", v.len()))?;

    let n_bits_bytes = header.n_bits_bytes();
    if n_bits_bytes.len() != 4 {
        return Err("nBits must be 4 bytes".to_string());
    }
    let bits_raw = u32::from_le_bytes(
        n_bits_bytes
            .try_into()
            .map_err(|_| "nBits must be 4 bytes".to_string())?,
    );
    let bits =
        CompactDifficulty::try_from_bits(bits_raw).map_err(|e| format!("invalid nBits: {e}"))?;

    let nonse: [u8; 32] = header
        .nonce()
        .try_into()
        .map_err(|v: Vec<u8>| format!("nonse must be 32 bytes, got {}", v.len()))?;

    let solution = EquihashSolution::try_from(header.solution()).map_err(|_| {
        format!(
            "solution must be 32 or 1344 bytes, got {}",
            header.solution().len()
        )
    })?;

    // Tree sizes are cumulative, so each block's size is its parent's plus the
    // notes this block adds. Counting them is why the transactions are
    // converted before the trees are built.
    let mut sapling_note_count = 0;
    let mut orchard_note_count = 0;
    let mut ironwood_note_count = 0;

    let full_transactions = full_block.transactions();
    let mut tx = Vec::with_capacity(full_transactions.len());

    for (i, ftx) in full_transactions.into_iter().enumerate() {
        let txdata = compact_tx_data_from_full_transaction(i as u64, ftx)
            .map_err(|e| format!("TxData conversion failed at index {i}: {e}"))?;

        sapling_note_count += txdata.sapling().outputs().len();
        orchard_note_count += txdata.orchard().actions().len();
        ironwood_note_count += txdata.ironwood().actions().len();

        tx.push(txdata);
    }

    let commitment_tree_data = CommitmentTreeData::new(
        CommitmentTreeRoots::new(final_sapling_root, final_orchard_root, final_ironwood_root),
        CommitmentTreeSizes::new(
            parent_sapling_size + sapling_note_count as u32,
            parent_orchard_size + orchard_note_count as u32,
            parent_ironwood_size + ironwood_note_count as u32,
        ),
    );

    let block_data = BlockData {
        version: header.version() as u32,
        time: header.time() as i64,
        merkle_root,
        block_commitments,
        bits,
        nonce: nonse,
        solution,
    };

    // Chainwork is cumulative too: this block's work added to its parent's.
    let block_work = block_data.bits.to_work();
    let chainwork = match parent_chainwork {
        Some(parent) => parent
            .add(&block_work)
            .map_err(|e| format!("chainwork overflow: {e}"))?,
        None => block_work,
    };

    let context = BlockContext::new(
        BlockHash::from(hash),
        BlockHash::from(parent_hash),
        chainwork,
        height,
    );

    Ok(IndexedBlock::new(
        context,
        block_data,
        tx,
        commitment_tree_data,
    ))
}
