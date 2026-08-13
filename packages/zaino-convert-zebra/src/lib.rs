//! Conversion: `zebra_chain` types → `zaino_primitives` domain types.
//!
//! Each function maps one zebra type to one domain type.
//! The `block_from_zebra` entry point composes them.

use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, EncryptedCiphertext,
    EphemeralKey, Height, MerkleRoot, NoteCommitment, Nullifier, OrchardAction, OrchardData,
    SaplingData, SaplingOutput, SaplingSpend, Script, SignedZatoshis, Transaction, TransactionId,
    TransparentData, TransparentInput, TransparentOutput, Zatoshis,
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
///
/// `chain_metadata` is passed whole rather than as loose tree sizes: they are
/// same-typed counts whose order carries no clue, so positional arguments could
/// be transposed silently. The caller supplies it because cumulative tree sizes
/// are indexed state, not present in the block itself.
pub fn block_from_zebra(
    zb: &zebra_chain::block::Block,
    chain_metadata: ChainMetadata,
) -> Result<Block, ConvertError> {
    Ok(Block {
        header: header_from_zebra(zb)?,
        transactions: zb
            .transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| transaction_from_zebra(tx, i as u32))
            .collect::<Result<Vec<_>, _>>()?,
        chain_metadata,
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

/// Convert one zebra transaction into the domain's.
///
/// `index` is the transaction's position within its block. A transaction with
/// no block — a mempool transaction — passes `0`, matching what the light-wallet
/// protocol serves for one.
///
/// Public because the mempool stream converts a single transaction rather than
/// a whole block; every other caller reaches this through
/// [`block_from_zebra`].
pub fn transaction_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
    index: u32,
) -> Result<Transaction, ConvertError> {
    Ok(Transaction {
        txid: TransactionId::from(tx.hash().0),
        index,
        transparent: transparent_from_zebra(tx)?,
        sapling: sapling_from_zebra(tx),
        orchard: orchard_from_zebra(tx),
        ironwood: ironwood_from_zebra(tx),
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
                prev_txid: TransactionId::from(outpoint.hash.0),
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
        value_balance: SignedZatoshis::new(i64::from(tx.sapling_value_balance().sapling_amount())),
    }
}

fn orchard_from_zebra(tx: &zebra_chain::transaction::Transaction) -> OrchardData {
    orchard_shaped_from_zebra(
        tx.orchard_actions(),
        i64::from(tx.orchard_value_balance().orchard_amount()),
    )
}

fn ironwood_from_zebra(tx: &zebra_chain::transaction::Transaction) -> OrchardData {
    orchard_shaped_from_zebra(
        tx.ironwood_actions(),
        i64::from(tx.ironwood_value_balance().ironwood_amount()),
    )
}

/// Convert an Orchard-shaped action stream and its value balance.
///
/// Shared by the Orchard and Ironwood pools: Ironwood actions are the same
/// `zebra_chain::orchard::Action` type, so the two differ only in which
/// accessors the caller reads them from. Keeping one conversion means a fix to
/// action handling cannot reach one pool and miss the other.
fn orchard_shaped_from_zebra<'a>(
    actions: impl Iterator<Item = &'a zebra_chain::orchard::Action>,
    value_balance: i64,
) -> OrchardData {
    OrchardData {
        actions: actions
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
        value_balance: SignedZatoshis::new(value_balance),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Orchard and Ironwood share [`orchard_shaped_from_zebra`], so the value
    /// balance must come from the caller rather than being read off a pool
    /// inside it — otherwise one pool's balance would be reported for both.
    #[test]
    fn shared_conversion_reports_the_balance_it_was_given() {
        let empty: [&zebra_chain::orchard::Action; 0] = [];

        let pool = orchard_shaped_from_zebra(empty.into_iter(), -42);

        assert!(pool.actions.is_empty());
        assert_eq!(pool.value_balance, SignedZatoshis::new(-42));
    }
}

/// Our reading of the consensus constants against zebra's.
///
/// `zaino-consensus` states these itself and depends on no node
/// implementation, because they are protocol facts rather than any
/// implementation's values. That independence is only safe if the two readings
/// are checked against each other somewhere, and this crate — which already
/// owns our relationship to zebra's types — is that somewhere.
///
/// A failure here does not say which side is wrong. It says the protocol moved
/// or one of us misread it, and that the answer needs looking up in the
/// specification rather than copied across.
#[cfg(test)]
mod consensus_agreement {
    #[test]
    fn coinbase_maturity_agrees() {
        assert_eq!(
            zaino_consensus::COINBASE_MATURITY,
            zebra_chain::transparent::MIN_TRANSPARENT_COINBASE_MATURITY
        );
    }

    #[test]
    fn reorg_limit_agrees() {
        assert_eq!(
            zaino_consensus::MAX_BLOCK_REORG_HEIGHT,
            zebra_chain::parameters::constants::MAX_BLOCK_REORG_HEIGHT
        );
    }

    #[test]
    fn max_block_bytes_agrees() {
        assert_eq!(
            zaino_consensus::MAX_BLOCK_BYTES,
            zebra_chain::block::MAX_BLOCK_BYTES
        );
    }

    /// `work_from_bits` is implemented against the specification rather than by
    /// delegating, so it is swept against zebra's implementation across the
    /// whole encoding space — every exponent, and mantissas chosen to sit on
    /// the boundaries where the two could plausibly disagree: zero, one, the
    /// byte and half-word limits the overflow rules key off, the sign bit that
    /// makes a target negative, and the largest valid magnitude.
    ///
    /// Agreement on rejection matters as much as agreement on value. A
    /// disagreement about *which* encodings are valid would let a block through
    /// that a validator refuses, or refuse one it accepts.
    #[test]
    fn work_from_bits_agrees_across_the_encoding_space() {
        const MANTISSAS: [u32; 8] = [
            0x00_0000, 0x00_0001, 0x00_00ff, 0x00_0100, 0x00_ffff, 0x01_0000, 0x7f_ffff, 0x80_0000,
        ];

        let mut compared = 0;
        for exponent in 0u32..=0xff {
            for mantissa in MANTISSAS {
                let bits = (exponent << 24) | mantissa;

                let ours = zaino_consensus::work_from_bits(bits).ok();
                let theirs =
                    zebra_chain::work::difficulty::CompactDifficulty::from_bytes_in_display_order(
                        &bits.to_be_bytes(),
                    )
                    .ok()
                    .and_then(|compact| compact.to_work())
                    .map(|work| work.as_u128());

                assert_eq!(ours, theirs, "disagreement at nBits {bits:#010x}");
                compared += 1;
            }
        }

        assert_eq!(compared, 256 * MANTISSAS.len());
    }
}
