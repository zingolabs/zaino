//! Conversion: `zebra_chain` types → `zaino_primitives` domain types.
//!
//! Each function maps one zebra type to one domain type.
//! The `block_from_zebra` entry point composes them.

use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, CompactCiphertext,
    CompactCiphertextLength, CompactDifficulty, CompactDifficultyError, EphemeralKey,
    EquihashSolution, Height, MerkleRoot, NoteCommitment, Nullifier, OrchardAction, OrchardData,
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
    /// The header's difficulty threshold failed the domain's validation.
    ///
    /// A zebra header holds an already-validated difficulty, so this only
    /// fires if the two implementations disagree about the acceptance set —
    /// exactly what this crate's differential tests pin down.
    #[error("difficulty: {0}")]
    Difficulty(#[from] CompactDifficultyError),
    /// A note ciphertext too short to contain the compact scanning prefix.
    ///
    /// Zebra hands over full 580-byte ciphertexts, so this only fires on
    /// corrupt data. A short ciphertext is failed loud rather than padded or
    /// sliced into a panic: no wallet could scan the block regardless, and
    /// inventing bytes would hide the corruption.
    #[error("ciphertext too short for the compact prefix: {0}")]
    Ciphertext(#[from] CompactCiphertextLength),
}

/// Take the 52-byte compact scanning prefix off a full note ciphertext.
///
/// Rejects a source shorter than the prefix with the length actually seen;
/// bytes past the prefix are the rest of the full ciphertext and are dropped.
fn compact_prefix(enc: &[u8]) -> Result<CompactCiphertext, ConvertError> {
    let head = enc.len().min(CompactCiphertext::LENGTH);
    Ok(CompactCiphertext::try_new(&enc[..head])?)
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
        version: h.version,
        prev_hash: BlockHash::from(h.previous_block_hash.0),
        height: Height::try_from(height.0).map_err(|e| ConvertError::Height(e.to_string()))?,
        time: h.time.timestamp() as u32,
        merkle_root: MerkleRoot::from(h.merkle_root.0),
        block_commitments: BlockCommitments::from(*h.commitment_bytes),
        // Zebra exposes no raw-bits accessor, so the value crosses as its
        // display-order bytes, through the primitives door of the same shape.
        bits: CompactDifficulty::try_from_be_bytes(
            h.difficulty_threshold.bytes_in_display_order(),
        )?,
        nonce: *h.nonce,
        solution: solution_from_zebra(h.solution),
    })
}

/// Convert a zebra Equihash solution into the domain's.
fn solution_from_zebra(solution: zebra_chain::work::equihash::Solution) -> EquihashSolution {
    match solution {
        zebra_chain::work::equihash::Solution::Common(bytes) => EquihashSolution::Standard(bytes),
        zebra_chain::work::equihash::Solution::Regtest(bytes) => EquihashSolution::Regtest(bytes),
    }
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
        version: header.version,
        prev_hash: BlockHash::from(header.previous_block_hash.0),
        height: Height::try_from(height.0).map_err(|e| ConvertError::Height(e.to_string()))?,
        time: header.time.timestamp() as u32,
        merkle_root: MerkleRoot::from(header.merkle_root.0),
        block_commitments: BlockCommitments::from(*header.commitment_bytes),
        bits: CompactDifficulty::try_from_be_bytes(
            header.difficulty_threshold.bytes_in_display_order(),
        )?,
        nonce: *header.nonce,
        solution: solution_from_zebra(header.solution),
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
        sapling: sapling_from_zebra(tx)?,
        orchard: orchard_from_zebra(tx)?,
        ironwood: ironwood_from_zebra(tx)?,
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

fn sapling_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
) -> Result<SaplingData, ConvertError> {
    Ok(SaplingData {
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
                Ok(SaplingOutput {
                    cmu: NoteCommitment::from(out.cm_u.to_bytes()),
                    ephemeral_key: EphemeralKey::from(epk_bytes),
                    enc_ciphertext: compact_prefix(&enc_bytes)?,
                })
            })
            .collect::<Result<Vec<_>, ConvertError>>()?,
        value_balance: SignedZatoshis::try_new(i64::from(
            tx.sapling_value_balance().sapling_amount(),
        ))
        .map_err(|e| ConvertError::Value(e.to_string()))?,
    })
}

fn orchard_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
) -> Result<OrchardData, ConvertError> {
    orchard_shaped_from_zebra(
        tx.orchard_actions(),
        i64::from(tx.orchard_value_balance().orchard_amount()),
    )
}

fn ironwood_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
) -> Result<OrchardData, ConvertError> {
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
) -> Result<OrchardData, ConvertError> {
    Ok(OrchardData {
        actions: actions
            .map(|act| {
                let nf_bytes: [u8; 32] = act.nullifier.into();
                let epk_bytes: [u8; 32] = (&act.ephemeral_key).into();
                let enc_bytes: [u8; 580] = act.enc_ciphertext.into();
                Ok(OrchardAction {
                    nullifier: Nullifier::from(nf_bytes),
                    cmx: NoteCommitment::from(<[u8; 32]>::from(act.cm_x)),
                    ephemeral_key: EphemeralKey::from(epk_bytes),
                    enc_ciphertext: compact_prefix(&enc_bytes)?,
                })
            })
            .collect::<Result<Vec<_>, ConvertError>>()?,
        value_balance: SignedZatoshis::try_new(value_balance)
            .map_err(|e| ConvertError::Value(e.to_string()))?,
    })
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

        let pool = orchard_shaped_from_zebra(empty.into_iter(), -42).expect("a valid balance");

        assert!(pool.actions.is_empty());
        assert_eq!(
            pool.value_balance,
            SignedZatoshis::try_new(-42).expect("a valid balance")
        );
    }

    /// A source ciphertext shorter than the compact prefix is a typed error.
    ///
    /// Regression test. The prefix take used to be an unchecked `[..52]`
    /// slice, which panics on short input; corrupt source data must instead
    /// surface as a `ConvertError` naming the length seen.
    #[test]
    fn a_short_ciphertext_is_a_typed_error_not_a_panic() {
        let err = compact_prefix(&[0u8; 51]).expect_err("51 bytes cannot fill the prefix");

        assert!(matches!(
            err,
            ConvertError::Ciphertext(CompactCiphertextLength { got: 51 })
        ));
    }

    /// A full-length ciphertext yields its 52-byte head, dropping the rest.
    #[test]
    fn a_full_ciphertext_yields_its_head() {
        let mut full = [0u8; 580];
        full[..52].copy_from_slice(&[0xcd; 52]);

        let prefix = compact_prefix(&full).expect("a full ciphertext always has a head");

        assert_eq!(<[u8; 52]>::from(prefix), [0xcd; 52]);
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
}

/// The primitives difficulty pipeline against zebra's, as differential oracle.
///
/// `zaino_primitives::types::CompactDifficulty` implements the whole
/// nBits → target → work conversion natively, against the specification. The
/// safety net for that independence is equality with a consensus
/// implementation on both of the pipeline's judgements:
///
/// - **the acceptance set** — which `u32` values are valid encodings. A
///   disagreement here would let a block through that a validator refuses, or
///   refuse one it accepts;
/// - **the work value** — including *when there is none*: our typed
///   over-width refusal must land exactly where zebra's `to_work` declines.
///
/// A failure does not say which side is wrong, only that the two readings of
/// the specification diverge and the answer needs looking up rather than
/// copied across.
#[cfg(test)]
mod difficulty_agreement {
    use core::num::NonZeroU128;

    use proptest::prelude::*;

    use zaino_primitives::types::CompactDifficulty;

    /// Both pipeline judgements at once: `None` for a rejected encoding,
    /// `Some(None)` for a valid encoding whose work does not fit `u128`,
    /// `Some(Some(work))` otherwise.
    fn primitives_view(bits: u32) -> Option<Option<u128>> {
        CompactDifficulty::try_from_bits(bits)
            .ok()
            .map(|cd| cd.to_work().ok().map(|work| NonZeroU128::from(work).get()))
    }

    /// Zebra's judgements in the same shape. Construction succeeds exactly
    /// when `to_expanded` accepts, and `to_work` is `None` on over-width.
    fn zebra_view(bits: u32) -> Option<Option<u128>> {
        zebra_chain::work::difficulty::CompactDifficulty::from_bytes_in_display_order(
            &bits.to_be_bytes(),
        )
        .ok()
        .map(|compact| compact.to_work().map(|work| work.as_u128()))
    }

    /// Sweeps the encoding space: every exponent, with mantissas chosen to sit
    /// on the boundaries where the two implementations could plausibly
    /// disagree — zero, one, the byte and half-word limits the overflow rules
    /// key off, the sign bit that makes a target negative, and the largest
    /// valid magnitude.
    #[test]
    fn pipeline_agrees_across_the_encoding_space() {
        const MANTISSAS: [u32; 8] = [
            0x00_0000, 0x00_0001, 0x00_00ff, 0x00_0100, 0x00_ffff, 0x01_0000, 0x7f_ffff, 0x80_0000,
        ];

        let mut compared = 0;
        for exponent in 0u32..=0xff {
            for mantissa in MANTISSAS {
                let bits = (exponent << 24) | mantissa;
                assert_eq!(
                    primitives_view(bits),
                    zebra_view(bits),
                    "disagreement at nBits {bits:#010x}"
                );
                compared += 1;
            }
        }

        assert_eq!(compared, 256 * MANTISSAS.len());
    }

    /// The specific edges the sweep's grid could miss, plus the rejection
    /// vectors the store's validated type historically pinned: all-zero, the
    /// sign bit, all-ones, the boundary exponents on both sides of their
    /// mantissa limits, an underflow to zero, and a valid-but-tiny target
    /// whose work exceeds 128 bits.
    #[test]
    fn pipeline_agrees_on_the_edge_vectors() {
        const EDGES: [u32; 14] = [
            0x0000_0000, // zero: no target
            0x0180_0000, // sign bit set: negative target
            u32::MAX,    // all ones
            0x0100_0100, // exponent underflow shifts the mantissa away
            0x0101_0000, // valid target of 1: work over 128 bits
            0x0300_0001, // unscaled target of 1: work over 128 bits
            0x2200_00ff, // boundary exponent, mantissa within a byte
            0x2200_0100, // boundary exponent, mantissa a bit too wide
            0x2100_ffff, // boundary exponent, mantissa within two bytes
            0x2101_0000, // boundary exponent, mantissa a bit too wide
            0x2300_0001, // exponent past every mantissa
            0x1f07_ffff, // mainnet proof-of-work limit (and genesis)
            0x2007_ffff, // testnet/regtest proof-of-work limit
            0x1d00_ffff, // classic minimum-difficulty encoding
        ];

        for bits in EDGES {
            assert_eq!(
                primitives_view(bits),
                zebra_view(bits),
                "disagreement at nBits {bits:#010x}"
            );
        }
    }

    /// Real header bits with their known work, pinned as literals so this
    /// suite still means something if both implementations drifted together.
    #[test]
    fn known_work_vectors() {
        // Zcash mainnet genesis (also the mainnet proof-of-work limit):
        // target 0x07ffff·256^28, work exactly 2^13.
        assert_eq!(primitives_view(0x1f07_ffff), Some(Some(8192)));
        // The testnet/regtest proof-of-work limit: target 0x07ffff·256^29.
        assert_eq!(primitives_view(0x2007_ffff), Some(Some(32)));
        // The Bitcoin-family minimum-difficulty encoding: target 0xffff·256^26.
        assert_eq!(primitives_view(0x1d00_ffff), Some(Some(0x1_0001_0001)));
    }

    proptest! {
        /// Acceptance-set and work equality over arbitrary bit patterns.
        #[test]
        fn pipeline_agrees_on_arbitrary_bits(bits in any::<u32>()) {
            prop_assert_eq!(
                primitives_view(bits),
                zebra_view(bits),
                "disagreement at nBits {:#010x}", bits
            );
        }
    }
}
