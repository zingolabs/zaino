//! Conversion: `zebra_chain` types → `zaino_primitives` domain types.
//!
//! Each function maps one zebra type to one domain type.
//! The `block_from_zebra` entry point composes them.

use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, EncryptedCiphertext,
    EphemeralKey, Height, MerkleRoot, NoteCommitment, Nullifier, OrchardAction, OrchardData,
    PreIndexCompactBlock, PreIndexCompactTx, SaplingData, SaplingOutput, SaplingSpend, Script,
    SignedZatoshis, Transaction, TransactionHash, TransparentData, TransparentInput,
    TransparentOutput, Zatoshis,
};

/// Errors during conversion from zebra types.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// Block height couldn't be extracted or validated.
    #[error("height: {0}")]
    Height(String),
    /// A value exceeded protocol limits.
    #[error("value overflow: {0}")]
    Value(String),
}

/// Convert a zebra block into a domain [`Block`].
pub fn block_from_zebra(
    zb: &zebra_chain::block::Block,
    sapling_tree_size: u32,
    orchard_tree_size: u32,
) -> Result<Block, ConvertError> {
    Ok(Block {
        header: header_from_zebra(&zb)?,
        transactions: zb
            .transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| transaction_from_zebra(tx, i as u32))
            .collect::<Result<Vec<_>, _>>()?,
        chain_metadata: ChainMetadata {
            sapling_tree_size,
            orchard_tree_size,
        },
    })
}

/// Convert just the header — skips all transaction parsing.
/// Much faster for header-only indexes on large blocks.
pub fn header_from_zebra(zb: &zebra_chain::block::Block) -> Result<BlockHeader, ConvertError> {
    let h = &zb.header;
    let height = zb
        .coinbase_height()
        .ok_or_else(|| ConvertError::Height("no coinbase height".into()))?;

    Ok(BlockHeader {
        hash: BlockHash::from(zb.hash().0),
        prev_hash: BlockHash::from(h.previous_block_hash.0),
        height: Height::try_from(height.0).map_err(|e| ConvertError::Height(e.to_string()))?,
        time: h.time.timestamp() as u32,
        merkle_root: MerkleRoot::from(h.merkle_root.0),
        block_commitments: BlockCommitments::from(*h.commitment_bytes),
        // TODO: upstream PR to zebra adding CompactDifficulty::to_bits() -> u32.
        // Workaround: round-trip through display-order bytes.
        bits: u32::from_be_bytes(h.difficulty_threshold.bytes_in_display_order()),
        nonce: *h.nonce,
    })
}

/// Convert from pre-parsed header components (from ReadRequest::BlockHeader).
/// No block deserialization needed at all.
pub fn header_from_parts(
    header: &zebra_chain::block::Header,
    hash: zebra_chain::block::Hash,
    height: zebra_chain::block::Height,
) -> Result<BlockHeader, ConvertError> {
    Ok(BlockHeader {
        hash: BlockHash::from(hash.0),
        prev_hash: BlockHash::from(header.previous_block_hash.0),
        height: Height::try_from(height.0).map_err(|e| ConvertError::Height(e.to_string()))?,
        time: header.time.timestamp() as u32,
        merkle_root: MerkleRoot::from(header.merkle_root.0),
        block_commitments: BlockCommitments::from(*header.commitment_bytes),
        bits: u32::from_be_bytes(header.difficulty_threshold.bytes_in_display_order()),
        nonce: *header.nonce,
    })
}

fn transaction_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
    index: u32,
) -> Result<Transaction, ConvertError> {
    Ok(Transaction {
        txid: TransactionHash::from(tx.hash().0),
        index,
        transparent: transparent_from_zebra(tx)?,
        sapling: sapling_from_zebra(tx),
        orchard: orchard_from_zebra(tx),
    })
}

fn transparent_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
) -> Result<TransparentData, ConvertError> {
    let inputs = tx
        .inputs()
        .iter()
        .filter_map(|input| match input {
            zebra_chain::transparent::Input::PrevOut { outpoint, .. } => Some(TransparentInput {
                prev_txid: TransactionHash::from(outpoint.hash.0),
                prev_index: outpoint.index,
            }),
            zebra_chain::transparent::Input::Coinbase { .. } => None,
        })
        .collect();

    let outputs = tx
        .outputs()
        .iter()
        .map(|out| {
            Ok(TransparentOutput {
                value: Zatoshis::new(u64::from(out.value))
                    .map_err(|e| ConvertError::Height(e.to_string()))?,
                script: Script::new(out.lock_script.as_raw_bytes().to_vec()),
            })
        })
        .collect::<Result<Vec<_>, ConvertError>>()?;

    Ok(TransparentData { inputs, outputs })
}

fn sapling_from_zebra(tx: &zebra_chain::transaction::Transaction) -> SaplingData {
    SaplingData {
        spends: tx
            .sapling_nullifiers()
            .map(|nf| SaplingSpend {
                nullifier: Nullifier::from(<[u8; 32]>::from(*nf)),
            })
            .collect(),
        outputs: tx
            .sapling_outputs()
            .map(|out| {
                let epk_bytes: [u8; 32] = (&out.ephemeral_key).into();
                let enc_bytes: [u8; 580] = out.enc_ciphertext.into();
                SaplingOutput {
                    cmu: NoteCommitment::from(out.cm_u.to_bytes()),
                    ephemeral_key: EphemeralKey::from(epk_bytes),
                    enc_ciphertext: EncryptedCiphertext::new(enc_bytes[..52].to_vec()),
                }
            })
            .collect(),
        value_balance: SignedZatoshis::new(
            i64::from(tx.sapling_value_balance().sapling_amount()),
        ),
    }
}

/// Convert a zebra compact block into a domain [`PreIndexCompactBlock`].
pub fn pre_index_compact_block_from_zebra(
    cb: &zebra_chain::transaction::compact::CompactBlock,
) -> PreIndexCompactBlock {
    PreIndexCompactBlock {
        hash: BlockHash::from(cb.hash.0),
        prev_hash: BlockHash::from(cb.header.previous_block_hash.0),
        height: cb.height.0,
        time: cb.header.time.timestamp() as u32,
        bits: u32::from_be_bytes(cb.header.difficulty_threshold.bytes_in_display_order()),
        transactions: cb
            .transactions
            .iter()
            .map(pre_index_compact_tx_from_zebra)
            .collect(),
    }
}

fn pre_index_compact_tx_from_zebra(
    ctx: &zebra_chain::transaction::compact::CompactTransaction,
) -> PreIndexCompactTx {
    PreIndexCompactTx {
        txid: TransactionHash::from(ctx.txid.0),
        transparent_inputs: ctx
            .transparent_inputs
            .iter()
            .map(|inp| TransparentInput {
                prev_txid: TransactionHash::from(inp.hash.0),
                prev_index: inp.index,
            })
            .collect(),
        transparent_outputs: ctx
            .transparent_outputs
            .iter()
            .map(|out| TransparentOutput {
                value: Zatoshis::new(out.value).expect("valid zatoshis from zebra"),
                script: Script::new(out.script.clone()),
            })
            .collect(),
        sapling_nullifiers: ctx
            .sapling_nullifiers
            .iter()
            .map(|nf| Nullifier::from(*nf))
            .collect(),
        sapling_outputs: ctx
            .sapling_outputs
            .iter()
            .map(|o| SaplingOutput {
                cmu: NoteCommitment::from(o.cmu),
                ephemeral_key: EphemeralKey::from(o.ephemeral_key),
                enc_ciphertext: EncryptedCiphertext::new(o.enc_ciphertext_head.to_vec()),
            })
            .collect(),
        orchard_actions: ctx
            .orchard_actions
            .iter()
            .map(|a| OrchardAction {
                nullifier: Nullifier::from(a.nullifier),
                cmx: NoteCommitment::from(a.cmx),
                ephemeral_key: EphemeralKey::from(a.ephemeral_key),
                enc_ciphertext: EncryptedCiphertext::new(a.enc_ciphertext_head.to_vec()),
            })
            .collect(),
    }
}

fn orchard_from_zebra(tx: &zebra_chain::transaction::Transaction) -> OrchardData {
    OrchardData {
        actions: tx
            .orchard_actions()
            .map(|act| {
                let nf_bytes: [u8; 32] = act.nullifier.into();
                let epk_bytes: [u8; 32] = (&act.ephemeral_key).into();
                let enc_bytes: [u8; 580] = act.enc_ciphertext.into();
                OrchardAction {
                    nullifier: Nullifier::from(nf_bytes),
                    cmx: NoteCommitment::from(<[u8; 32]>::from(act.cm_x)),
                    ephemeral_key: EphemeralKey::from(epk_bytes),
                    enc_ciphertext: EncryptedCiphertext::new(enc_bytes[..52].to_vec()),
                }
            })
            .collect(),
        value_balance: SignedZatoshis::new(
            i64::from(tx.orchard_value_balance().orchard_amount()),
        ),
    }
}
