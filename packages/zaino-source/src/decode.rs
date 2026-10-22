//! Consensus bytes → domain types: `getblock <h> 0` blocks and standalone mempool transactions
//!
//! - librustzcash's lazy readers (`CompressedTransaction`): txid for every version, curve points
//!   left compressed (nothing here needs them decompressed)
//! - The wire boundary: every malformed input is a typed `DecodeError`, never a panic

use std::io::{self, Cursor};

use zaino_primitives::types::{
    Block, BlockRef, CompactCiphertext, CompactDifficulty, CompactDifficultyError, DecodedHeader,
    EphemeralKey, HeaderBytes, HeaderError, NoteCommitment, Nullifier, OrchardAction, OrchardData,
    SaplingData, SaplingOutput, SaplingSpend, SproutData, Transaction, TransparentData,
    TransparentOutput,
};
use zcash_encoding::CompactSize;
use zcash_primitives::transaction::{CompressedTransaction, TxVersion};
use zcash_protocol::consensus::{BlockHeight, BranchId};
use zcash_protocol::{value::ZatBalance, BlockHash, TxId};

use crate::{FailureMode, NonDomainError, QueryError};

type OrchardBytes = orchard::BundleBytes<orchard::bundle::Authorized, ZatBalance>;

/// Bytes that do not decode into a domain block or transaction (one variant per rejection)
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("malformed consensus bytes: {0}")]
    Malformed(#[from] io::Error),
    #[error("{0} trailing bytes after the last transaction")]
    Trailing(usize),
    #[error("block has no transactions")]
    NoTransactions,
    #[error("transaction 0 is not a coinbase")]
    NoCoinbase,
    #[error("coinbase carries no BIP 34 height")]
    NoCoinbaseHeight,
    #[error("negative header version {0}")]
    NegativeVersion(i32),
    #[error("sprout JoinSplit values sum past the money supply")]
    SproutBalance,
    #[error("header: {0}")]
    Header(#[from] HeaderError),
    #[error("difficulty: {0}")]
    Difficulty(#[from] CompactDifficultyError),
    #[error("expiry height {0} above ZIP-203's 499999999")]
    ExpiryHeight(u32),
}

/// ZIP-203: `nExpiryHeight` ≤ this, or the transaction is invalid on its own
const MAX_EXPIRY_HEIGHT: u32 = 499_999_999;

/// `getblock <h> 0` / p2p block bytes → [`Block`] (only block parse, every index projects from it)
pub fn decode_block(raw: &[u8]) -> Result<Block, DecodeError> {
    let (header, body) = HeaderBytes::split(raw)?;
    let mut cursor = Cursor::new(body);
    let count = CompactSize::read(&mut cursor)?;
    let transactions =
        (0..count).map(|_| read_transaction(&mut cursor)).collect::<Result<Vec<_>, _>>()?;
    reject_trailing(&cursor)?;

    let coinbase = transactions.first().ok_or(DecodeError::NoTransactions)?;
    if !coinbase.transparent_bundle().is_some_and(|bundle| bundle.is_coinbase()) {
        return Err(DecodeError::NoCoinbase);
    }
    let header = block_header(&header, coinbase)?;
    let transactions = transactions.iter().map(transaction).collect::<Result<Vec<_>, _>>()?;
    Ok(Block::new(header, transactions))
}

/// What submission checks before any push, from the bytes alone (a peer answers a push with
/// nothing, not even the txid)
///
/// - `expiry_height` `None` = never expires (0 on the wire, or pre-Overwinter)
/// - `branch` = v5+'s embedded consensus branch id (earlier versions carry none)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prepared {
    pub txid: TxId,
    pub expiry_height: Option<BlockHeight>,
    pub branch: Option<BranchId>,
}

pub fn prepare_transaction(raw: &[u8]) -> Result<Prepared, DecodeError> {
    let tx = read_whole(raw)?;
    let embeds_branch =
        !matches!(tx.version(), TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4);
    let expiry = u32::from(tx.expiry_height());
    if expiry > MAX_EXPIRY_HEIGHT {
        return Err(DecodeError::ExpiryHeight(expiry));
    }
    Ok(Prepared {
        txid: tx.txid(),
        expiry_height: (expiry != 0).then(|| BlockHeight::from(expiry)),
        branch: embeds_branch.then(|| tx.consensus_branch_id()),
    })
}

/// One standalone transaction's bytes (mempool: no block, so no position) → [`Transaction`]
pub fn decode_transaction(raw: &[u8]) -> Result<Transaction, DecodeError> {
    transaction(&read_whole(raw)?)
}

/// Validator-served block bytes → [`Block`] (undecodable = the validator's bytes, `Parse`)
pub(crate) fn served_block<E>(raw: &[u8]) -> Result<Block, QueryError<E>>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    decode_block(raw).map_err(|e| NonDomainError::from_cause(FailureMode::Parse, e).into())
}

/// - Branch id = read from the tx itself for v5+
/// - v1–v4 only store it (txid = hash of the bytes)
fn read_transaction(cursor: &mut Cursor<&[u8]>) -> io::Result<CompressedTransaction> {
    CompressedTransaction::read(cursor, BranchId::Sprout)
}

/// `raw` = exactly one transaction
fn read_whole(raw: &[u8]) -> Result<CompressedTransaction, DecodeError> {
    let mut cursor = Cursor::new(raw);
    let tx = read_transaction(&mut cursor)?;
    reject_trailing(&cursor)?;
    Ok(tx)
}

fn reject_trailing(cursor: &Cursor<&[u8]>) -> Result<(), DecodeError> {
    match cursor.get_ref().len() - cursor.position() as usize {
        0 => Ok(()),
        trailing => Err(DecodeError::Trailing(trailing)),
    }
}

/// `coinbase` = transaction 0, already checked to be a coinbase
fn block_header(
    raw: &HeaderBytes<'_>,
    coinbase: &CompressedTransaction,
) -> Result<DecodedHeader, DecodeError> {
    let version = raw.version();
    Ok(DecodedHeader {
        hash: raw.hash(),
        version: u32::try_from(version).map_err(|_| DecodeError::NegativeVersion(version))?,
        prev_hash: raw.prev_hash(),
        height: block_height(raw, coinbase)?,
        time: raw.time(),
        merkle_root: raw.merkle_root(),
        block_commitments: raw.block_commitments(),
        bits: CompactDifficulty::try_from_bits(raw.bits())?,
        nonce: raw.nonce(),
        solution: raw.equihash_solution(),
    })
}

/// - Genesis = height 0 (its coinbase predates BIP 34)
/// - Every later block's coinbase opens with its height
fn block_height(
    raw: &HeaderBytes<'_>,
    coinbase: &CompressedTransaction,
) -> Result<BlockHeight, DecodeError> {
    let height = if raw.prev_hash() == BlockHash([0; 32]) {
        0
    } else {
        coinbase
            .transparent_bundle()
            .and_then(|bundle| bundle.vin.first())
            .and_then(|input| bip34_height(&input.script_sig().0 .0))
            .ok_or(DecodeError::NoCoinbaseHeight)?
    };
    Ok(BlockHeight::from(height))
}

/// BIP 34 height push: `OP_1`..`OP_16`, or 1–4 little-endian bytes (minimal `CScriptNum`)
fn bip34_height(script: &[u8]) -> Option<u32> {
    match *script.first()? {
        op @ 0x51..=0x60 => Some(u32::from(op - 0x50)),
        len @ 1..=4 => {
            let mut le = [0u8; 4];
            le[..usize::from(len)].copy_from_slice(script.get(1..=usize::from(len))?);
            Some(u32::from_le_bytes(le))
        }
        _ => None,
    }
}

fn transaction(tx: &CompressedTransaction) -> Result<Transaction, DecodeError> {
    Ok(Transaction {
        txid: tx.txid(),
        transparent: transparent(tx)?,
        sprout: sprout(tx)?,
        sapling: sapling(tx)?,
        orchard: orchard_shaped(tx.orchard_bundle())?,
        ironwood: orchard_shaped(tx.ironwood_bundle())?,
    })
}

fn transparent(tx: &CompressedTransaction) -> Result<TransparentData, DecodeError> {
    let Some(bundle) = tx.transparent_bundle() else {
        return Ok(TransparentData::default());
    };
    let coinbase = bundle.is_coinbase();
    // coinbase input: null prevout, no value (protocol.pdf#coinbasetransactions §3.11)
    let inputs = match coinbase {
        true => Vec::new(),
        false => bundle.vin.iter().map(|input| input.prevout().clone()).collect(),
    };
    let outputs = bundle
        .vout
        .iter()
        .map(|output| TransparentOutput {
            value: output.value(),
            script: output.script_pubkey().clone(),
        })
        .collect();
    Ok(TransparentData { coinbase, inputs, outputs })
}

fn sprout(tx: &CompressedTransaction) -> Result<SproutData, DecodeError> {
    let Some(bundle) = tx.sprout_bundle() else {
        return Ok(SproutData::default());
    };
    // Σ net_value, net_value = vpub_new − vpub_old (protocol.pdf#joinsplit §3.5)
    let balance = bundle.value_balance().ok_or(DecodeError::SproutBalance)?;
    Ok(SproutData { value_balance: balance })
}

fn sapling(tx: &CompressedTransaction) -> Result<SaplingData, DecodeError> {
    let Some(bundle) = tx.sapling_bundle() else {
        return Ok(SaplingData::default());
    };
    Ok(SaplingData {
        spends: bundle
            .shielded_spends()
            .iter()
            .map(|spend| SaplingSpend { nullifier: Nullifier(spend.nullifier().0) })
            .collect(),
        outputs: bundle
            .shielded_outputs()
            .iter()
            .map(|output| {
                Ok(SaplingOutput {
                    cmu: NoteCommitment(output.cmu().to_bytes()),
                    ephemeral_key: EphemeralKey(output.ephemeral_key().0),
                    enc_ciphertext: CompactCiphertext::prefix_of(output.enc_ciphertext()),
                })
            })
            .collect::<Result<Vec<_>, DecodeError>>()?,
        // v4 without spends or outputs → 0 (protocol.pdf#txnconsensus, zip-0225)
        value_balance: *bundle.value_balance(),
    })
}

/// Orchard + Ironwood: one bundle shape (`BundleBytes`), so one projection
fn orchard_shaped(bundle: Option<&OrchardBytes>) -> Result<OrchardData, DecodeError> {
    let Some(bundle) = bundle else {
        return Ok(OrchardData::default());
    };
    Ok(OrchardData {
        actions: bundle
            .actions()
            .iter()
            .map(|action| {
                let note = action.encrypted_note();
                Ok(OrchardAction {
                    nullifier: Nullifier(action.nullifier().to_bytes()),
                    cmx: NoteCommitment(action.cmx().to_bytes()),
                    ephemeral_key: EphemeralKey(note.epk_bytes),
                    enc_ciphertext: CompactCiphertext::prefix_of(&note.enc_ciphertext.0),
                })
            })
            .collect::<Result<Vec<_>, DecodeError>>()?,
        value_balance: *bundle.value_balance(),
    })
}

/// Byte span of each tx inside a block (header + count skipped by the same readers)
pub(crate) fn tx_spans(raw: &[u8]) -> Result<Vec<std::ops::Range<usize>>, DecodeError> {
    let (header, body) = HeaderBytes::split(raw)?;
    let at = header.as_bytes().len();
    let mut cursor = Cursor::new(body);
    let count = CompactSize::read(&mut cursor)?;
    (0..count)
        .map(|_| {
            let start = at + cursor.position() as usize;
            read_transaction(&mut cursor)?;
            Ok(start..at + cursor.position() as usize)
        })
        .collect()
}

/// Transaction `index`'s own bytes inside block `raw`, iff that block is `at` (hash, coinbase
/// height) with the body its header commits to and that transaction is `txid` (`None` = any
/// other bytes: a misanswer)
///
/// - v5 authorizing data (proofs, signatures) unchecked: committed only through
///   `hashBlockCommitments`, never the merkle root
pub fn mined_transaction(raw: &[u8], at: BlockRef, index: u32, txid: TxId) -> Option<Vec<u8>> {
    let block = decode_block(raw).ok()?;
    let header = block.header();
    let ours = header.hash == at.hash && header.height == at.height && block.check_body().is_ok();
    let index = usize::try_from(index).ok()?;
    let mined = block.transactions().get(index).is_some_and(|tx| tx.txid == txid);
    let span = tx_spans(raw).ok()?.into_iter().nth(index)?;
    (ours && mined).then(|| raw[span].to_vec())
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use proptest::{prop_assert, prop_assert_eq};
    use zcash_primitives::block::BlockHeader;
    use zcash_primitives::transaction::Transaction as Librustzcash;

    use super::*;
    use crate::testing::fixtures::{self, HEIGHTS};
    use crate::testing::raw_transaction;

    /// Mainnet 2,000,000 (every pool): each tx decoded alone = its decode inside the block
    #[test]
    fn each_tx_of_a_mainnet_block_decodes_alone_identically() {
        let raw = fixtures::block(2_000_000);

        let block = decode_block(&raw).expect("block decodes");

        let spans = tx_spans(&raw).expect("spans");
        assert_eq!(spans.len(), block.transactions().len(), "one span per transaction");
        for (index, (span, in_block)) in spans.into_iter().zip(block.transactions()).enumerate() {
            let alone = decode_transaction(&raw[span]).expect("standalone decodes");
            let txid = in_block.txid;
            assert_eq!(format!("{alone:?}"), format!("{in_block:?}"), "tx {index} ({txid})");
        }
    }

    #[test]
    fn garbage_and_trailing_bytes_are_refused() {
        let raw = fixtures::block(2_000_000);
        let first = tx_spans(&raw).expect("spans")[0].clone();

        let garbage = decode_transaction(&[0u8; 8]);
        let padded_tx = decode_transaction(&[&raw[first], &[0]].concat());
        let padded_block = decode_block(&[&raw[..], &[0]].concat());

        assert!(matches!(garbage, Err(DecodeError::Malformed(_))), "8 zero bytes: {garbage:?}");
        let padded_tx = padded_tx.map(|tx| tx.txid);
        assert!(matches!(padded_tx, Err(DecodeError::Trailing(1))), "tx + 1 byte: {padded_tx:?}");
        let padded_block = padded_block.map(|block| block.block_ref());
        assert!(
            matches!(padded_block, Err(DecodeError::Trailing(1))),
            "block + 1 byte: {padded_block:?}"
        );
    }

    /// Mainnet 2,000,000: each transaction cut from its block by position = its own bytes
    #[test]
    fn a_mined_transaction_comes_out_of_its_block_by_position() {
        let raw = fixtures::block(2_000_000);
        let block = decode_block(&raw).expect("block decodes");
        let at = block.block_ref();

        let own = fixtures::transactions(2_000_000);

        for (index, tx) in (0u32..).zip(block.transactions()) {
            let cut = mined_transaction(&raw, at, index, tx.txid);
            let matches = cut.as_deref() == own.get(index as usize).map(Vec::as_slice);
            assert!(matches, "tx {index} ({}): cut bytes = its own", tx.txid);
        }
    }

    /// Nothing for another block, another txid, a body its header does not commit to, a position
    /// past the last tx
    #[test]
    fn a_mined_transaction_is_absent_unless_block_txid_body_and_position_match() {
        const INSIDE_TX_ONE: usize = 40;
        let raw = fixtures::block(2_000_000);
        let block = decode_block(&raw).expect("block decodes");
        let (at, txs) = (block.block_ref(), block.transactions());
        let mut tampered = raw.clone();
        tampered[tx_spans(&raw).expect("spans")[1].start + INSIDE_TX_ONE] ^= 1;
        let elsewhere = BlockRef { height: at.height + 1, ..at };
        let past_last = u32::try_from(txs.len()).expect("a small block");

        let absent = |cut: Option<Vec<u8>>| cut.map(|bytes| bytes.len());
        assert_eq!(absent(mined_transaction(&raw, elsewhere, 1, txs[1].txid)), None, "other block");
        assert_eq!(absent(mined_transaction(&raw, at, 1, txs[2].txid)), None, "tx 2's txid at 1");
        assert_eq!(absent(mined_transaction(&tampered, at, 1, txs[1].txid)), None, "body edited");
        assert_eq!(absent(mined_transaction(&raw, at, past_last, txs[1].txid)), None, "past last");
    }

    const PREV_HASH: Range<usize> = 4..36;

    /// Header prefix located by librustzcash's reader (independent of `HeaderBytes::split`)
    #[test]
    fn each_fixture_header_hashes_links_reencodes_and_roots_its_txids() {
        for height in HEIGHTS {
            let raw = fixtures::block(height);
            let mut cursor = Cursor::new(raw.as_slice());
            BlockHeader::read(&mut cursor).expect("header");
            let header_bytes = &raw[..cursor.position() as usize];

            let decoded = decode_block(&raw).expect("block decodes");

            let header = decoded.header();
            let sha256d = BlockHash(zaino_primitives::sha256d(header_bytes));
            assert_eq!(header.hash, sha256d, "{height}: hash = SHA-256d of the header bytes");
            let prev: [u8; 32] = header_bytes[PREV_HASH].try_into().expect("32 bytes");
            assert_eq!(header.prev_hash, BlockHash(prev), "{height}: prev_hash at bytes 4..36");
            let reencoded = crate::testing::encode_header(header);
            assert!(reencoded == header_bytes, "{height}: encode_header round-trips the header");
            assert_eq!(decoded.check_body(), Ok(()), "{height}: txids rebuild the merkle root");
        }
    }

    /// v4 tx with two JoinSplits (3 000 → 10 000, then 500 → 0): decoded Sprout balance =
    /// Σ vpub_new − Σ vpub_old = 6 500 (no fixture block and no `arb_tx` carries JoinSplits)
    #[test]
    fn sprout_value_balance_nets_every_joinsplit() {
        use zcash_primitives::transaction::{
            components::sprout::{Bundle, JsDescription, SproutProof, NOTE_CIPHERTEXT_SIZE},
            Authorized, TransactionData, TxVersion,
        };
        use zcash_protocol::value::Zatoshis;

        let joinsplit = |old: u64, new: u64| {
            JsDescription::from_parts(
                Zatoshis::from_u64(old).expect("in range"),
                Zatoshis::from_u64(new).expect("in range"),
                [1; 32],
                [[2; 32]; 2],
                [[3; 32]; 2],
                [4; 32],
                [5; 32],
                [[6; 32]; 2],
                SproutProof::Groth([7; 192]),
                [[8; NOTE_CIPHERTEXT_SIZE]; 2],
            )
        };
        let tx = TransactionData::<Authorized>::from_parts(
            TxVersion::V4,
            BranchId::Canopy,
            0,
            0.into(),
            None,
            Some(Bundle {
                joinsplits: vec![joinsplit(3_000, 10_000), joinsplit(500, 0)],
                joinsplit_pubkey: [9; 32],
                joinsplit_sig: [10; 64],
            }),
            None,
            None,
        )
        .freeze()
        .expect("v4 freezes");
        let mut raw = Vec::new();
        tx.write(&mut raw).expect("writes");

        let decoded = decode_transaction(&raw).expect("decodes");

        let balance = i64::from(decoded.sprout.value_balance);
        assert_eq!(balance, 6_500, "(10 000 + 0) − (3 000 + 500)");
    }

    /// ZIP 203: expiry ≤ 499 999 999 prepares, one above is refused naming it
    #[test]
    fn expiry_at_the_zip_203_ceiling_prepares_and_one_above_is_refused() {
        let (_, at_ceiling) = raw_transaction(0, MAX_EXPIRY_HEIGHT);
        let (_, above) = raw_transaction(0, MAX_EXPIRY_HEIGHT + 1);

        let at_ceiling = prepare_transaction(&at_ceiling).map(|prepared| prepared.expiry_height);
        let above = prepare_transaction(&above).map(|prepared| prepared.expiry_height);

        let ceiling = BlockHeight::from(MAX_EXPIRY_HEIGHT);
        assert!(matches!(at_ceiling, Ok(Some(e)) if e == ceiling), "at ceiling: {at_ceiling:?}");
        assert!(
            matches!(above, Err(DecodeError::ExpiryHeight(e)) if e == MAX_EXPIRY_HEIGHT + 1),
            "ceiling + 1: {above:?}"
        );
    }

    /// `arb_tx` draw, its Debug a one-line summary (a full `Debug` = pages per shrink step)
    struct Arb(Librustzcash);

    impl std::fmt::Debug for Arb {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let tx = &self.0;
            let transparent =
                tx.transparent_bundle().map_or((0, 0), |b| (b.vin.len(), b.vout.len()));
            let sapling = tx
                .sapling_bundle()
                .map_or((0, 0), |b| (b.shielded_spends().len(), b.shielded_outputs().len()));
            let orchard = tx.orchard_bundle().map_or(0, |b| b.actions().len());
            let ironwood = tx.ironwood_bundle().map_or(0, |b| b.actions().len());
            write!(
                f,
                "{:?} {:?} txid {} expiry {}: vin/vout {transparent:?}, sapling spends/outputs \
                 {sapling:?}, orchard actions {orchard}, ironwood actions {ironwood}",
                tx.version(),
                tx.consensus_branch_id(),
                tx.txid(),
                u32::from(tx.expiry_height()),
            )
        }
    }

    fn arb_any_served_format() -> impl proptest::strategy::Strategy<Value = Arb> {
        use proptest::strategy::Strategy;
        use zcash_primitives::transaction::testing::arb_tx;
        proptest::prop_oneof![
            arb_tx(BranchId::Sapling),
            arb_tx(BranchId::Nu5),
            arb_tx(BranchId::Nu6_3),
        ]
        .prop_map(Arb)
    }

    fn written(tx: &Librustzcash) -> Vec<u8> {
        let mut raw = Vec::new();
        tx.write(&mut raw).expect("a Vec takes every write");
        raw
    }

    /// `(nullifier, cmx)` per action + the bundle's value balance
    type Actions = (Vec<([u8; 32], [u8; 32])>, i64);

    proptest::proptest! {
        // arb_tx filters ~80 draws per case (signing-key prop_filter): reject cap scales w/ cases
        #![proptest_config(proptest::prelude::ProptestConfig {
            max_local_rejects: u32::MAX,
            ..proptest::prelude::ProptestConfig::with_cases(64)
        })]

        /// librustzcash-written txs of every format Zaino serves (Sapling v4, NU5 v5, NU6.3 v5 +
        /// v6 with Ironwood) decode to their source
        #[test]
        fn transactions_decode_to_their_source(tx in arb_any_served_format()) {
            let (tx, raw) = (&tx.0, written(&tx.0));

            let decoded = decode_transaction(&raw).expect("decodes");

            // bytes = the truth: librustzcash's reference parse of them = txid oracle
            let reference = Librustzcash::read(&raw[..], tx.consensus_branch_id())
                .expect("librustzcash reads its own bytes");
            prop_assert_eq!(decoded.txid, reference.txid(), "txid");
            let vout: Vec<u64> =
                decoded.transparent.outputs.iter().map(|o| o.value.into_u64()).collect();
            let source_vout: Vec<u64> = tx
                .transparent_bundle()
                .map_or(vec![], |b| b.vout.iter().map(|o| o.value().into_u64()).collect());
            prop_assert_eq!(vout, source_vout, "transparent output values");
            let sapling = tx.sapling_bundle();
            let spends: Vec<_> = decoded.sapling.spends.iter().map(|s| s.nullifier.0).collect();
            let source_spends: Vec<_> = sapling
                .map_or(vec![], |b| b.shielded_spends().iter().map(|s| s.nullifier().0).collect());
            prop_assert_eq!(spends, source_spends, "sapling spend nullifiers");
            let cmus: Vec<_> = decoded.sapling.outputs.iter().map(|o| o.cmu.0).collect();
            let source_cmus: Vec<_> = sapling.map_or(vec![], |b| {
                b.shielded_outputs().iter().map(|o| o.cmu().to_bytes()).collect()
            });
            prop_assert_eq!(cmus, source_cmus, "sapling output cmus");
            let sapling_balance = sapling.map_or(0, |b| i64::from(*b.value_balance()));
            let decoded_balance = i64::from(decoded.sapling.value_balance);
            prop_assert_eq!(decoded_balance, sapling_balance, "sapling value balance");
            let source_actions = |bundle: Option<&orchard::Bundle<_, ZatBalance>>| -> Actions {
                let actions = bundle.map_or(vec![], |b| {
                    b.actions().iter().map(|a| (a.nullifier().to_bytes(), a.cmx().to_bytes())).collect()
                });
                (actions, bundle.map_or(0, |b| i64::from(*b.value_balance())))
            };
            let decoded_actions = |pool: &OrchardData| -> Actions {
                let actions = pool.actions.iter().map(|a| (a.nullifier.0, a.cmx.0)).collect();
                (actions, i64::from(pool.value_balance))
            };
            prop_assert_eq!(
                decoded_actions(&decoded.orchard),
                source_actions(tx.orchard_bundle()),
                "orchard (nullifier, cmx) per action, value balance"
            );
            prop_assert_eq!(
                decoded_actions(&decoded.ironwood),
                source_actions(tx.ironwood_bundle()),
                "ironwood (nullifier, cmx) per action, value balance"
            );
        }

        /// - submission facts = the source's own: txid, expiry (`0` = none), branch id only where
        ///   the format embeds one
        /// - refused: expiry past ZIP 203's ceiling, trailing or truncated bytes
        #[test]
        fn a_prepared_transaction_carries_its_own_txid_expiry_and_branch(
            tx in arb_any_served_format()
        ) {
            let (tx, raw) = (&tx.0, written(&tx.0));
            let expiry = u32::from(tx.expiry_height());

            let prepared = prepare_transaction(&raw);

            if expiry > MAX_EXPIRY_HEIGHT {
                let refused = prepared.map(|p| p.txid);
                let named = matches!(refused, Err(DecodeError::ExpiryHeight(e)) if e == expiry);
                prop_assert!(named, "expiry {} past the ceiling: {:?}", expiry, refused);
                return Ok(());
            }
            let prepared = prepared.expect("expiry within ZIP 203: prepares");
            let reference = Librustzcash::read(&raw[..], tx.consensus_branch_id())
                .expect("librustzcash reads its own bytes");
            prop_assert_eq!(prepared.txid, reference.txid(), "txid");
            let stated = (expiry != 0).then_some(expiry);
            prop_assert_eq!(prepared.expiry_height.map(u32::from), stated, "expiry (0 = none)");
            let embeds = matches!(tx.version(), TxVersion::V5 | TxVersion::V6);
            let branch = embeds.then(|| tx.consensus_branch_id());
            prop_assert_eq!(prepared.branch, branch, "branch id, embedded by v5 / v6 only");
            let padded = prepare_transaction(&[&raw[..], &[0]].concat()).map(|p| p.txid);
            let padded_named = matches!(padded, Err(DecodeError::Trailing(1)));
            prop_assert!(padded_named, "padded by 1 byte: {:?}", padded);
            let truncated = prepare_transaction(&raw[..raw.len() - 1]).map(|p| p.txid);
            let truncated_named = matches!(truncated, Err(DecodeError::Malformed(_)));
            prop_assert!(truncated_named, "last byte cut: {:?}", truncated);
        }
    }

    /// Push forms zcashd/zebra emit: `OP_n` for 1–16, minimal LE bytes above (sign byte at 128)
    #[test]
    fn bip34_heights_decode_in_every_push_form() {
        #[rustfmt::skip]
        let cases: [(&[u8], Option<u32>); 8] = [
            (&[0x51],                         Some(1)),
            (&[0x60],                         Some(16)),
            (&[0x01, 0x11],                   Some(17)),
            (&[0x02, 0x80, 0x00],             Some(128)),
            (&[0x03, 0x40, 0x42, 0x0f],       Some(1_000_000)),
            (&[0x04, 0x2f, 0x4f, 0x34, 0x00], Some(3_428_143)),
            (&[0x03, 0x40, 0x42],             None),
            (&[0x00],                         None),
        ];
        for (script, height) in cases {
            assert_eq!(bip34_height(script), height, "script {script:02x?}");
        }
    }
}
