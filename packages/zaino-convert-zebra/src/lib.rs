//! Conversion: `zebra_chain` types → `zaino_primitives` domain types.
//!
//! Each function maps one zebra type to one domain type.
//! The `block_from_zebra` entry point composes them.

use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, CoinbaseInput,
    CompactCiphertext, CompactCiphertextLength, CompactDifficulty, CompactDifficultyError,
    EphemeralKey, EquihashSolution, Height, JoinSplitValues, MerkleRoot, NoteCommitment, Nullifier,
    OrchardAction, OrchardData, PreIndexCompactBlock, PreIndexCompactTx, SaplingData,
    SaplingOutput, SaplingSpend, Script, SignedZatoshis, Transaction, TransactionDetail,
    TransactionId, TransparentData, TransparentInput, TransparentInputDetail, TransparentOutput,
    Zatoshis,
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
    /// The converted transactions did not form a valid block.
    #[error("block: {0}")]
    Block(String),
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
    let header = header_from_zebra(zb)?;
    let transactions = zb
        .transactions
        .iter()
        .map(|tx| transaction_from_zebra(tx))
        .collect::<Result<Vec<_>, _>>()?;
    Block::try_new(header, transactions, chain_metadata)
        .map_err(|e| ConvertError::Block(e.to_string()))
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
/// A transaction carries no position: its slot in a block, and so whether it is
/// the coinbase, is the block's to know (see [`block_from_zebra`], which reads
/// it from order). A mempool transaction is in no block and has no position to
/// invent.
///
/// Public because the mempool stream converts a single transaction rather than
/// a whole block; every other caller reaches this through
/// [`block_from_zebra`].
pub fn transaction_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
) -> Result<Transaction, ConvertError> {
    Ok(Transaction {
        txid: TransactionId::from(tx.hash().0),
        transparent: transparent_from_zebra(tx)?,
        sapling: sapling_from_zebra(tx)?,
        orchard: orchard_from_zebra(tx)?,
        ironwood: ironwood_from_zebra(tx)?,
    })
}

/// Build the facts the indexing [`Transaction`] drops: the envelope, the
/// coinbase input, and the Sprout pool values.
///
/// Pairs with [`transaction_from_zebra`]: that yields the indexing shape, this
/// yields what the explorer surface additionally needs, from the same zebra
/// transaction. `size` is the serialized byte length; the caller holds the
/// bytes, so it passes the length rather than re-serializing here.
pub fn transaction_detail_from_zebra(
    tx: &zebra_chain::transaction::Transaction,
    size: u64,
) -> Result<TransactionDetail, ConvertError> {
    let coinbase = match tx.inputs().first() {
        Some(input @ zebra_chain::transparent::Input::Coinbase { sequence, .. }) => {
            // `coinbase_script()` reconstructs the scriptSig (the BIP-34 height
            // prefix plus miner data, or the fixed genesis data), and is `None`
            // only for a genesis-height input whose data is not the genesis
            // scriptSig — a malformed coinbase the source should never yield.
            let script = input.coinbase_script().ok_or_else(|| {
                ConvertError::Block("coinbase script could not be reconstructed".into())
            })?;
            Some(CoinbaseInput {
                script: Script::new(script),
                sequence: *sequence,
            })
        }
        _ => None,
    };

    // The signature script and sequence of every non-coinbase transparent input,
    // in input order — the facts the indexing input shape drops but the explorer's
    // `scriptSig`/`sequence` need. A coinbase input is carried by `coinbase` above,
    // so it is skipped here, keeping this list aligned 1:1 with the indexing
    // `transaction.transparent.inputs`.
    let transparent_inputs = tx
        .inputs()
        .iter()
        .filter_map(|input| match input {
            zebra_chain::transparent::Input::PrevOut {
                unlock_script,
                sequence,
                ..
            } => Some(TransparentInputDetail {
                script_sig: Script::new(unlock_script.as_raw_bytes().to_vec()),
                sequence: *sequence,
            }),
            zebra_chain::transparent::Input::Coinbase { .. } => None,
        })
        .collect();

    let joinsplits = tx
        .sprout_joinsplits()
        .map(|js| {
            Ok(JoinSplitValues {
                vpub_old: Zatoshis::new(u64::from(js.vpub_old))
                    .map_err(|e| ConvertError::Value(e.to_string()))?,
                vpub_new: Zatoshis::new(u64::from(js.vpub_new))
                    .map_err(|e| ConvertError::Value(e.to_string()))?,
            })
        })
        .collect::<Result<Vec<_>, ConvertError>>()?;

    // The Overwinter flag — not `expiry_height()` — decides whether an expiry is
    // present. zebra's `expiry_height()` collapses a zero expiry to `None` on
    // v3+, but zcashd still emits `expiryheight: 0` for an overwintered
    // transaction with no expiry, and that must stay distinct from a
    // pre-Overwinter transaction, which has no expiry field at all.
    let expiry_height = if tx.is_overwintered() {
        let value = tx.expiry_height().map_or(0, |h| h.0);
        Some(Height::try_from(value).map_err(|e| ConvertError::Height(e.to_string()))?)
    } else {
        None
    };

    Ok(TransactionDetail {
        version: tx.version(),
        overwintered: tx.is_overwintered(),
        version_group_id: tx.version_group_id(),
        lock_time: tx.raw_lock_time(),
        expiry_height,
        size,
        coinbase,
        transparent_inputs,
        joinsplits,
    })
}

/// Convert the fork's compact block into the domain's pre-index compact block —
/// the source path the indexer reads.
///
/// The fork's `ReadRequest::CompactBlock` projects each transaction to compact
/// form — transparent outpoints and outputs, shielded nullifiers, note
/// commitments, ephemeral keys, and the compact ciphertext head — dropping
/// proofs, signatures, and input scripts. It parses with zebra's own
/// deserializer, so the id and every field are correct for every transaction
/// version (the v5 ZIP-244 id, the Ironwood pool). The header conversion is
/// shared with the full path via [`header_from_parts`], so hash/prev-hash/time/
/// difficulty endianness cannot drift between the two.
pub fn pre_index_compact_block_from_zebra(
    compact: &zebra_chain::transaction::compact::CompactBlock,
) -> Result<PreIndexCompactBlock, ConvertError> {
    let header = header_from_parts(&compact.header, compact.hash, compact.height)?;
    let transactions = compact
        .transactions
        .iter()
        .map(compact_tx_from_zebra)
        .collect::<Result<Vec<_>, ConvertError>>()?;
    Ok(PreIndexCompactBlock {
        hash: header.hash,
        prev_hash: header.prev_hash,
        height: u32::from(header.height),
        time: header.time,
        bits: header.bits,
        transactions,
    })
}

/// Convert one compact transaction from the zebra fork into the domain's.
fn compact_tx_from_zebra(
    tx: &zebra_chain::transaction::compact::CompactTransaction,
) -> Result<PreIndexCompactTx, ConvertError> {
    let transparent_inputs = tx
        .transparent_inputs
        .iter()
        .map(|outpoint| TransparentInput {
            prev_txid: TransactionId::from(outpoint.hash.0),
            prev_index: outpoint.index,
        })
        .collect();
    let transparent_outputs = tx
        .transparent_outputs
        .iter()
        .map(|out| {
            Ok(TransparentOutput {
                value: Zatoshis::new(out.value).map_err(|e| ConvertError::Value(e.to_string()))?,
                script: Script::new(out.script.clone()),
            })
        })
        .collect::<Result<Vec<_>, ConvertError>>()?;
    let sapling_outputs = tx
        .sapling_outputs
        .iter()
        .map(|out| SaplingOutput {
            cmu: NoteCommitment::from(out.cmu),
            ephemeral_key: EphemeralKey::from(out.ephemeral_key),
            enc_ciphertext: CompactCiphertext::from(out.enc_ciphertext_head),
        })
        .collect();
    let compact_action =
        |act: &zebra_chain::transaction::compact::CompactOrchardAction| OrchardAction {
            nullifier: Nullifier::from(act.nullifier),
            cmx: NoteCommitment::from(act.cmx),
            ephemeral_key: EphemeralKey::from(act.ephemeral_key),
            enc_ciphertext: CompactCiphertext::from(act.enc_ciphertext_head),
        };
    let orchard_actions = tx.orchard_actions.iter().map(compact_action).collect();
    let ironwood_actions = tx.ironwood_actions.iter().map(compact_action).collect();
    Ok(PreIndexCompactTx {
        txid: TransactionId::from(tx.txid.0),
        transparent_inputs,
        transparent_outputs,
        sapling_nullifiers: tx
            .sapling_nullifiers
            .iter()
            .map(|nf| Nullifier::from(*nf))
            .collect(),
        sapling_outputs,
        orchard_actions,
        ironwood_actions,
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

#[cfg(test)]
mod transaction_detail_tests {
    use super::*;
    use zaino_primitives::types::{CoinbaseInput, Height, JoinSplitValues};
    use zebra_chain::amount::{Amount, NonNegative};
    use zebra_chain::block::Height as ZebraHeight;
    use zebra_chain::parameters::NetworkUpgrade;
    use zebra_chain::primitives::{ed25519, x25519, Bctv14Proof};
    use zebra_chain::sprout;
    use zebra_chain::transaction::{JoinSplitData, LockTime, Transaction as ZebraTransaction};
    use zebra_chain::transparent;

    // Zcash's genesis coinbase scriptSig, the 77 bytes zcashd emits as the
    // genesis `coinbase` hex (zebra's `GENESIS_COINBASE_SCRIPT_SIG`). Reproduced
    // here because the constant is crate-private to zebra-chain.
    const GENESIS_COINBASE_SCRIPT_SIG: [u8; 77] = [
        4, 255, 255, 7, 31, 1, 4, 69, 90, 99, 97, 115, 104, 48, 98, 57, 99, 52, 101, 101, 102, 56,
        98, 55, 99, 99, 52, 49, 55, 101, 101, 53, 48, 48, 49, 101, 51, 53, 48, 48, 57, 56, 52, 98,
        54, 102, 101, 97, 51, 53, 54, 56, 51, 97, 55, 99, 97, 99, 49, 52, 49, 97, 48, 52, 51, 99,
        52, 50, 48, 54, 52, 56, 51, 53, 100, 51, 52,
    ];

    const SAPLING_VERSION_GROUP_ID: u32 = 0x892F_2085;

    fn amount(value: u64) -> Amount<NonNegative> {
        Amount::try_from(i64::try_from(value).expect("fits i64")).expect("a valid amount")
    }

    fn sprout_joinsplit(vpub_old: u64, vpub_new: u64) -> sprout::JoinSplit<Bctv14Proof> {
        sprout::JoinSplit {
            vpub_old: amount(vpub_old),
            vpub_new: amount(vpub_new),
            anchor: sprout::tree::Root::from([0u8; 32]),
            nullifiers: [
                sprout::note::Nullifier::from([0u8; 32]),
                sprout::note::Nullifier::from([1u8; 32]),
            ],
            commitments: [
                sprout::NoteCommitment::from([0u8; 32]),
                sprout::NoteCommitment::from([1u8; 32]),
            ],
            ephemeral_key: x25519::PublicKey::from([0u8; 32]),
            random_seed: sprout::RandomSeed::from([0u8; 32]),
            vmacs: [
                sprout::note::Mac::from([0u8; 32]),
                sprout::note::Mac::from([1u8; 32]),
            ],
            zkproof: Bctv14Proof([0u8; 296]),
            enc_ciphertexts: [
                sprout::note::EncryptedNote([0u8; 601]),
                sprout::note::EncryptedNote([0u8; 601]),
            ],
        }
    }

    /// A v4 transparent transaction: the envelope passes through unchanged, with
    /// no coinbase and no Sprout movement.
    #[test]
    fn v4_transparent_carries_its_envelope() {
        let tx = ZebraTransaction::V4 {
            inputs: vec![],
            outputs: vec![],
            lock_time: LockTime::Height(ZebraHeight(500)),
            expiry_height: ZebraHeight(999),
            joinsplit_data: None,
            sapling_shielded_data: None,
        };

        let detail = transaction_detail_from_zebra(&tx, 321).expect("a valid v4 detail");

        assert_eq!(detail.version, 4);
        assert!(detail.overwintered);
        assert_eq!(detail.version_group_id, Some(SAPLING_VERSION_GROUP_ID));
        assert_eq!(detail.lock_time, 500);
        assert_eq!(detail.lock_time, tx.raw_lock_time());
        assert_eq!(
            detail.expiry_height,
            Some(Height::try_from(999u32).expect("a valid height"))
        );
        assert_eq!(detail.coinbase, None);
        assert!(detail.joinsplits.is_empty());
    }

    /// An overwintered transaction with no expiry keeps `Some(Height(0))`, so
    /// `expiryheight: 0` renders — zebra's `expiry_height()` collapses it to
    /// `None`, which would erase zcashd's shape.
    #[test]
    fn overwintered_zero_expiry_is_some_zero() {
        let tx = ZebraTransaction::V4 {
            inputs: vec![],
            outputs: vec![],
            lock_time: LockTime::unlocked(),
            expiry_height: ZebraHeight(0),
            joinsplit_data: None,
            sapling_shielded_data: None,
        };

        let detail = transaction_detail_from_zebra(&tx, 1).expect("a valid v4 detail");

        assert_eq!(detail.expiry_height, Some(Height::GENESIS));
    }

    /// A non-genesis coinbase: the script is the reconstructed scriptSig and the
    /// sequence is carried through, with no transparent prevout to resolve.
    #[test]
    fn coinbase_carries_script_and_sequence() {
        let input = transparent::Input::Coinbase {
            height: ZebraHeight(100),
            data: vec![0x01, 0x02, 0x03],
            sequence: 0xffff_fffe,
        };
        let expected_script = input
            .coinbase_script()
            .expect("a non-genesis coinbase script");
        let tx = ZebraTransaction::V5 {
            network_upgrade: NetworkUpgrade::Nu5,
            lock_time: LockTime::unlocked(),
            expiry_height: ZebraHeight(0),
            inputs: vec![input],
            outputs: vec![],
            sapling_shielded_data: None,
            orchard_shielded_data: None,
        };

        let detail = transaction_detail_from_zebra(&tx, 64).expect("a valid coinbase detail");

        assert_eq!(
            detail.coinbase,
            Some(CoinbaseInput {
                script: Script::new(expected_script),
                sequence: 0xffff_fffe,
            })
        );
    }

    /// The genesis coinbase: its scriptSig is the fixed 77-byte genesis data,
    /// exactly what zcashd emits, which zebra's `coinbase_script()` special-cases.
    #[test]
    fn genesis_coinbase_is_the_genesis_script_sig() {
        let input = transparent::Input::Coinbase {
            height: ZebraHeight(0),
            data: GENESIS_COINBASE_SCRIPT_SIG.to_vec(),
            sequence: 0xffff_ffff,
        };
        let tx = ZebraTransaction::V5 {
            network_upgrade: NetworkUpgrade::Genesis,
            lock_time: LockTime::unlocked(),
            expiry_height: ZebraHeight(0),
            inputs: vec![input],
            outputs: vec![],
            sapling_shielded_data: None,
            orchard_shielded_data: None,
        };

        let detail = transaction_detail_from_zebra(&tx, 128).expect("a valid genesis detail");

        let coinbase = detail.coinbase.expect("genesis is a coinbase");
        assert_eq!(
            coinbase.script,
            Script::new(GENESIS_COINBASE_SCRIPT_SIG.to_vec())
        );
    }

    /// A v2 Sprout transaction: each JoinSplit's `vpub_old`/`vpub_new`, in order.
    #[test]
    fn v2_sprout_joinsplits_in_order() {
        let joinsplit_data = JoinSplitData {
            first: sprout_joinsplit(10, 20),
            rest: vec![sprout_joinsplit(30, 40)],
            pub_key: ed25519::VerificationKeyBytes::from([0u8; 32]),
            sig: ed25519::Signature::from([0u8; 64]),
        };
        let tx = ZebraTransaction::V2 {
            inputs: vec![],
            outputs: vec![],
            lock_time: LockTime::unlocked(),
            joinsplit_data: Some(joinsplit_data),
        };

        let detail = transaction_detail_from_zebra(&tx, 999).expect("a valid v2 detail");

        assert!(!detail.overwintered);
        assert_eq!(detail.version_group_id, None);
        assert_eq!(detail.expiry_height, None);
        assert_eq!(
            detail.joinsplits,
            vec![
                JoinSplitValues {
                    vpub_old: Zatoshis::new(10).expect("valid"),
                    vpub_new: Zatoshis::new(20).expect("valid"),
                },
                JoinSplitValues {
                    vpub_old: Zatoshis::new(30).expect("valid"),
                    vpub_new: Zatoshis::new(40).expect("valid"),
                },
            ]
        );
    }

    /// A v1 transaction: pre-Overwinter, so no version group id and no expiry.
    #[test]
    fn v1_has_no_overwinter_fields() {
        let tx = ZebraTransaction::V1 {
            inputs: vec![],
            outputs: vec![],
            lock_time: LockTime::unlocked(),
        };

        let detail = transaction_detail_from_zebra(&tx, 60).expect("a valid v1 detail");

        assert_eq!(detail.version, 1);
        assert!(!detail.overwintered);
        assert_eq!(detail.version_group_id, None);
        assert_eq!(detail.expiry_height, None);
        assert_eq!(detail.coinbase, None);
        assert!(detail.joinsplits.is_empty());
    }

    /// `size` is the caller's byte length, carried through verbatim.
    #[test]
    fn size_is_passed_through_verbatim() {
        let tx = ZebraTransaction::V1 {
            inputs: vec![],
            outputs: vec![],
            lock_time: LockTime::unlocked(),
        };

        let detail = transaction_detail_from_zebra(&tx, 4_242).expect("a valid detail");

        assert_eq!(detail.size, 4_242);
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

    use zaino_primitives::types::{CompactDifficulty, CompactDifficultyError};

    /// Both pipeline judgements at once: `None` for a rejected encoding,
    /// `Some(None)` for a well-formed target whose work does not fit `u128`,
    /// `Some(Some(work))` otherwise.
    fn primitives_view(bits: u32) -> Option<Option<u128>> {
        match CompactDifficulty::try_from_bits(bits) {
            Ok(cd) => Some(Some(NonZeroU128::from(cd.to_work()).get())),
            Err(CompactDifficultyError::WorkOverWidth { .. }) => Some(None),
            Err(
                CompactDifficultyError::NegativeTarget { .. }
                | CompactDifficultyError::ZeroTarget { .. }
                | CompactDifficultyError::OverflowTarget { .. },
            ) => None,
        }
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
