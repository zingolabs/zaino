//! Writing: what the domain hands over, as this backend stores it.
//!
//! The conversions a write takes on its way in, and the read-back that must
//! undo them exactly. A block that made the round trip through this store has
//! to produce the same rows as one that arrived from a validator, so both
//! directions of a pair live here together.
//!
//! The opposite direction is `to_domain`.

use zaino_chain_store::{ChainStoreError, StoredBlock, StoredTx};
use zaino_primitives::types::{
    BlockHash as DomainBlockHash, BlockTxPosition, Height as DomainHeight, OrchardAction,
    Outpoint as DomainOutpoint, ScriptType, SignedZatoshis,
};

use crate::types::{
    BlockHash, CompactTxData, Height, IndexedBlock, Outpoint, TransactionHash,
    TransparentCompactTx, TxLocation,
};

/// The domain's height, as this crate names it.
///
/// Infallible: every domain height is a `u32`.
pub(super) fn stored_height(height: DomainHeight) -> Height {
    Height(u32::from(height))
}

/// The same 32 bytes, as this crate names them.
pub(super) fn stored_hash(hash: DomainBlockHash) -> BlockHash {
    BlockHash(hash.into())
}

/// A domain position, as a stored transaction location.
///
/// `None` when the index exceeds what the stored form can hold. The stored
/// location keys a transaction by a `u16` index, so a position beyond that
/// names nothing on disk — which is an answer, not an error.
pub(super) fn tx_location(position: BlockTxPosition) -> Option<TxLocation> {
    let tx_index = u16::try_from(position.tx_index).ok()?;
    Some(TxLocation::new(u32::from(position.height), tx_index))
}

/// The same 32 bytes and index, as this crate names them.
pub(super) fn stored_outpoint(outpoint: &DomainOutpoint) -> Outpoint {
    Outpoint::new(outpoint.txid.into(), outpoint.index)
}

/// A domain block, as the shape the writer takes.
///
/// The reverse of `stored_block`, and not quite its inverse: a block that
/// arrives from a composer was built from a validator's, so its transparent
/// outputs carry real locking scripts, which are classified here exactly as the
/// source-driven build path classifies them. A block that made the round trip
/// out of this store carries reconstructed scripts, which classify back to the
/// same key — so both origins produce the same rows.
///
/// Public because ChainIndex reads blocks through
/// [`zaino_chain_store::StoredBlockRead`] and still answers
/// its callers in [`IndexedBlock`], which is also the shape its chain-head
/// adapter produces. One conversion serving both directions is what keeps a
/// block from changing shape as it crosses the finalised seam. It goes private
/// again when `IndexedBlock` stops being ChainIndex's block.
pub fn indexed_block_from_stored(block: &StoredBlock) -> Result<IndexedBlock, ChainStoreError> {
    let header = &block.header;
    let hash = stored_hash(header.hash);

    let context = crate::types::BlockContext::new(
        hash,
        stored_hash(header.prev_hash),
        Some(block.chainwork),
        Height(u32::from(header.height)),
    );

    // Shared with the write direction rather than restated: both start from the
    // same `BlockHeader`, so a second copy would be the same mapping free to
    // drift. The mapping is total — every fallible field, difficulty included,
    // is already validated by the types the header carries.
    let data = crate::conversion::block_data(header);

    let transactions = block
        .transactions
        .iter()
        .enumerate()
        .map(|(index, tx)| stored_compact_tx_data(index, tx, hash))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(IndexedBlock::new(
        context,
        data,
        transactions,
        crate::conversion::commitment_tree_data(&block.tree_roots),
    ))
}

/// One domain transaction, as the shape the writer stores.
///
/// The transaction at index 0 gets the coinbase's null prevout if it does not
/// already carry one. A block from a validator has had it dropped; a block read
/// back out of this store still has it. Both must produce the same rows,
/// because the input is a persisted field.
fn stored_compact_tx_data(
    index: usize,
    stored: &StoredTx,
    block: BlockHash,
) -> Result<CompactTxData, ChainStoreError> {
    let tx = &stored.compact;
    let mut inputs: Vec<crate::types::TxInCompact> = Vec::new();
    let carries_null_prevout = tx
        .transparent_inputs
        .first()
        .is_some_and(|input| <[u8; 32]>::from(input.prev_txid) == [0u8; 32]);
    if index == 0 && !carries_null_prevout {
        inputs.push(crate::types::TxInCompact::null_prevout());
    }
    inputs.extend(
        tx.transparent_inputs
            .iter()
            .map(|input| crate::types::TxInCompact::new(input.prev_txid.into(), input.prev_index)),
    );

    let outputs = tx
        .transparent_outputs
        .iter()
        .map(|output| {
            let script: Vec<u8> = output.script.clone().into();
            let (hash, script_type) = zaino_primitives::types::classify_script(&script);
            crate::types::TxOutCompact::new(
                u64::from(output.value),
                hash,
                stored_script_tag(script_type),
            )
            .ok_or_else(|| {
                ChainStoreError::backend(format!(
                    "block {block} has a transparent output that cannot be stored"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CompactTxData::new(
        index as u64,
        TransactionHash(tx.txid.into()),
        TransparentCompactTx::new(inputs, outputs),
        crate::types::SaplingCompactTx::new(
            stored.sapling_value.map(i64::from),
            tx.sapling_nullifiers
                .iter()
                .map(|nullifier| crate::types::CompactSaplingSpend::new((*nullifier).into()))
                .collect(),
            tx.sapling_outputs
                .iter()
                .map(|output| {
                    crate::types::CompactSaplingOutput::new(
                        output.cmu.into(),
                        output.ephemeral_key.into(),
                        output.enc_ciphertext.into(),
                    )
                })
                .collect(),
        ),
        stored_orchard(stored.orchard_value, &tx.orchard_actions),
        stored_orchard(stored.ironwood_value, &tx.ironwood_actions),
    ))
}

/// One shielded pool's stored compact form.
///
/// Takes the value balance rather than defaulting it: it is a persisted field,
/// and a `None` written where the row held a balance is a row this store
/// rewrote while only meaning to read it.
fn stored_orchard(
    value: Option<SignedZatoshis>,
    actions: &[OrchardAction],
) -> crate::types::OrchardCompactTx {
    crate::types::OrchardCompactTx::new(
        value.map(i64::from),
        actions
            .iter()
            .map(|action| {
                crate::types::CompactOrchardAction::new(
                    action.nullifier.into(),
                    action.cmx.into(),
                    action.ephemeral_key.into(),
                    action.enc_ciphertext.into(),
                )
            })
            .collect(),
    )
}

pub(super) fn stored_script_tag(script_type: ScriptType) -> u8 {
    match script_type {
        ScriptType::P2PKH => crate::types::ScriptType::P2PKH as u8,
        ScriptType::P2SH => crate::types::ScriptType::P2SH as u8,
        ScriptType::NonStandard => crate::types::ScriptType::NonStandard as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A position past what the stored form can key is an answer, not an error.
    ///
    /// The store keys a transaction by a `u16` index where the domain uses a
    /// `u32`, so a position beyond that names nothing on disk. Asking about it
    /// is a reasonable question with the answer "nothing there".
    #[test]
    fn a_position_beyond_the_stored_index_width_names_nothing() {
        let height = DomainHeight::try_from(1).expect("valid height");
        assert!(tx_location(BlockTxPosition {
            height,
            tx_index: u32::from(u16::MAX),
        })
        .is_some());
        assert!(tx_location(BlockTxPosition {
            height,
            tx_index: u32::from(u16::MAX) + 1,
        })
        .is_none());
    }
}
