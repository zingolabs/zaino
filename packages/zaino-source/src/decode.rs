//! Consensus bytes → domain types: `getblock <h> 0` blocks, `getblockheader <h> false` links and
//! standalone mempool transactions
//!
//! - librustzcash's lazy readers (`CompressedTransaction`): txid for every version, curve points
//!   left compressed (nothing here needs them decompressed)
//! - The wire boundary: every malformed input is a typed `DecodeError`, never a panic

use std::io::{self, Cursor};

use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, CompactCiphertext, CompactDifficulty,
    CompactDifficultyError, EphemeralKey, EquihashSolution, Height, HeightOverflow, MerkleRoot,
    NoteCommitment, Nullifier, OrchardAction, OrchardData, OutPoint, SaplingData, SaplingOutput,
    SaplingSpend, Script, SignedZatoshis, SignedZatoshisOverflow, SproutData, Transaction,
    TransactionId, TransparentData, TransparentOutput, Zatoshis, ZatoshisOverflow,
};
use zcash_encoding::CompactSize;
use zcash_primitives::{block::BlockHeader as RawHeader, transaction::CompressedTransaction};
use zcash_protocol::{consensus::BranchId, value::ZatBalance};

use crate::BlockLink;

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
    #[error("height: {0}")]
    Height(#[from] HeightOverflow),
    #[error("negative header version {0}")]
    NegativeVersion(i32),
    #[error("output value: {0}")]
    OutputValue(#[from] ZatoshisOverflow),
    #[error("value balance: {0}")]
    ValueBalance(#[from] SignedZatoshisOverflow),
    #[error("sprout JoinSplit values sum past the money supply")]
    SproutBalance,
    #[error("equihash solution of {0} bytes (neither 1344 nor regtest's 36)")]
    Solution(usize),
    #[error("difficulty: {0}")]
    Difficulty(#[from] CompactDifficultyError),
}

/// `getblock <h> 0` bytes → [`Block`] (the one block parse; every index projects from it)
pub(crate) fn block(raw: &[u8]) -> Result<Block, DecodeError> {
    let mut cursor = Cursor::new(raw);
    let header = RawHeader::read(&mut cursor)?;
    let count = CompactSize::read(&mut cursor)?;
    let transactions =
        (0..count).map(|_| read_transaction(&mut cursor)).collect::<Result<Vec<_>, _>>()?;
    let trailing = raw.len() - cursor.position() as usize;
    if trailing != 0 {
        return Err(DecodeError::Trailing(trailing));
    }

    let coinbase = transactions.first().ok_or(DecodeError::NoTransactions)?;
    if !coinbase.transparent_bundle().is_some_and(|bundle| bundle.is_coinbase()) {
        return Err(DecodeError::NoCoinbase);
    }
    let header = block_header(&header, coinbase)?;
    let transactions = transactions.iter().map(transaction).collect::<Result<Vec<_>, _>>()?;
    Ok(Block::new(header, transactions))
}

/// `getblockheader <h> false` bytes → [`BlockLink`] (hash recomputed, never taken on trust)
pub(crate) fn block_link(raw: &[u8]) -> Result<BlockLink, DecodeError> {
    let mut cursor = Cursor::new(raw);
    let header = RawHeader::read(&mut cursor)?;
    let trailing = raw.len() - cursor.position() as usize;
    if trailing != 0 {
        return Err(DecodeError::Trailing(trailing));
    }
    Ok(BlockLink {
        hash: BlockHash::from(header.hash().0),
        prev_hash: BlockHash::from(header.prev_block.0),
    })
}

/// One standalone transaction's bytes (mempool: no block, so no position) → [`Transaction`]
pub fn decode_transaction(raw: &[u8]) -> Result<Transaction, DecodeError> {
    let mut cursor = Cursor::new(raw);
    let tx = read_transaction(&mut cursor)?;
    let trailing = raw.len() - cursor.position() as usize;
    if trailing != 0 {
        return Err(DecodeError::Trailing(trailing));
    }
    transaction(&tx)
}

/// Branch id = read from the tx itself for v5+; v1–v4 only store it (txid = hash of the bytes)
fn read_transaction(cursor: &mut Cursor<&[u8]>) -> io::Result<CompressedTransaction> {
    CompressedTransaction::read(cursor, BranchId::Sprout)
}

/// `coinbase` = transaction 0, already checked to be a coinbase
fn block_header(
    raw: &RawHeader,
    coinbase: &CompressedTransaction,
) -> Result<BlockHeader, DecodeError> {
    let solution = if let Ok(standard) = <[u8; 1344]>::try_from(raw.solution.as_slice()) {
        EquihashSolution::Standard(standard)
    } else if let Ok(regtest) = <[u8; 36]>::try_from(raw.solution.as_slice()) {
        EquihashSolution::Regtest(regtest)
    } else {
        return Err(DecodeError::Solution(raw.solution.len()));
    };

    Ok(BlockHeader {
        hash: BlockHash::from(raw.hash().0),
        version: u32::try_from(raw.version)
            .map_err(|_| DecodeError::NegativeVersion(raw.version))?,
        prev_hash: BlockHash::from(raw.prev_block.0),
        height: block_height(raw, coinbase)?,
        time: raw.time,
        merkle_root: MerkleRoot::from(raw.merkle_root),
        block_commitments: BlockCommitments::from(raw.final_sapling_root),
        bits: CompactDifficulty::try_from_bits(raw.bits)?,
        nonce: raw.nonce,
        solution,
    })
}

/// Genesis = height 0 (its coinbase predates BIP 34); every later block's coinbase opens with it
fn block_height(raw: &RawHeader, coinbase: &CompressedTransaction) -> Result<Height, DecodeError> {
    let height = if BlockHash::from(raw.prev_block.0) == BlockHash::ZERO {
        0
    } else {
        coinbase
            .transparent_bundle()
            .and_then(|bundle| bundle.vin.first())
            .and_then(|input| bip34_height(&input.script_sig().0 .0))
            .ok_or(DecodeError::NoCoinbaseHeight)?
    };
    Ok(Height::try_from(height)?)
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
        txid: TransactionId::from(*tx.txid().as_ref()),
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
        false => bundle
            .vin
            .iter()
            .map(|input| OutPoint {
                txid: TransactionId::from(*input.prevout().hash()),
                vout: input.prevout().n(),
            })
            .collect(),
    };
    let outputs = bundle
        .vout
        .iter()
        .map(|output| {
            Ok(TransparentOutput {
                value: Zatoshis::new(output.value().into_u64())?,
                script: Script::new(output.script_pubkey().0 .0.clone()),
            })
        })
        .collect::<Result<Vec<_>, DecodeError>>()?;
    Ok(TransparentData { coinbase, inputs, outputs })
}

fn sprout(tx: &CompressedTransaction) -> Result<SproutData, DecodeError> {
    let Some(bundle) = tx.sprout_bundle() else {
        return Ok(SproutData::default());
    };
    // Σ net_value, net_value = vpub_new − vpub_old (protocol.pdf#joinsplit §3.5)
    let balance = bundle.value_balance().ok_or(DecodeError::SproutBalance)?;
    Ok(SproutData { value_balance: signed(balance)? })
}

fn sapling(tx: &CompressedTransaction) -> Result<SaplingData, DecodeError> {
    let Some(bundle) = tx.sapling_bundle() else {
        return Ok(SaplingData::default());
    };
    Ok(SaplingData {
        spends: bundle
            .shielded_spends()
            .iter()
            .map(|spend| SaplingSpend { nullifier: Nullifier::from(spend.nullifier().0) })
            .collect(),
        outputs: bundle
            .shielded_outputs()
            .iter()
            .map(|output| {
                Ok(SaplingOutput {
                    cmu: NoteCommitment::from(output.cmu().to_bytes()),
                    ephemeral_key: EphemeralKey::from(output.ephemeral_key().0),
                    enc_ciphertext: CompactCiphertext::prefix_of(output.enc_ciphertext()),
                })
            })
            .collect::<Result<Vec<_>, DecodeError>>()?,
        // v4 without spends or outputs → 0 (protocol.pdf#txnconsensus; zip-0225)
        value_balance: signed(*bundle.value_balance())?,
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
                    nullifier: Nullifier::from(action.nullifier().to_bytes()),
                    cmx: NoteCommitment::from(action.cmx().to_bytes()),
                    ephemeral_key: EphemeralKey::from(note.epk_bytes),
                    enc_ciphertext: CompactCiphertext::prefix_of(&note.enc_ciphertext.0),
                })
            })
            .collect::<Result<Vec<_>, DecodeError>>()?,
        value_balance: signed(*bundle.value_balance())?,
    })
}

fn signed(balance: ZatBalance) -> Result<SignedZatoshis, DecodeError> {
    Ok(SignedZatoshis::new(i64::from(balance))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(height: u32) -> Vec<u8> {
        let path = format!("{}/tests/fixtures/block_{height}.hex", env!("CARGO_MANIFEST_DIR"));
        let hex = std::fs::read_to_string(&path).expect("fixture readable");
        const_hex::decode(hex.trim()).expect("fixture is hex")
    }

    /// Byte span of each tx inside a block (header + count skipped by the same readers)
    fn tx_spans(raw: &[u8]) -> Vec<std::ops::Range<usize>> {
        let mut cursor = Cursor::new(raw);
        RawHeader::read(&mut cursor).expect("header");
        let count = CompactSize::read(&mut cursor).expect("count");
        (0..count)
            .map(|_| {
                let start = cursor.position() as usize;
                read_transaction(&mut cursor).expect("tx");
                start..cursor.position() as usize
            })
            .collect()
    }

    /// Mainnet 2,000,000 (44 txs: transparent in/out, sapling spends + outputs, orchard):
    /// header decoded, BIP 34 height agrees, every pool present, standalone path = block path
    #[test]
    fn a_mainnet_block_decodes_and_each_tx_decodes_alone_identically() {
        let raw = fixture(2_000_000);
        let block = block(&raw).expect("block decodes");

        assert_eq!(u32::from(block.header().height), 2_000_000);
        assert!(matches!(block.header().solution, EquihashSolution::Standard(_)));

        let standalone: Vec<Transaction> = tx_spans(&raw)
            .into_iter()
            .map(|span| decode_transaction(&raw[span]).expect("standalone decodes"))
            .collect();
        assert_eq!(standalone.len(), block.transactions().len());
        for (alone, mined) in standalone.iter().zip(block.transactions()) {
            assert_eq!(format!("{alone:?}"), format!("{mined:?}"), "tx {:?}", mined.txid);
        }

        let totals = standalone.iter().fold([0usize; 5], |mut acc, tx| {
            acc[0] += tx.transparent.inputs.len();
            acc[1] += tx.transparent.outputs.len();
            acc[2] += tx.sapling.spends.len();
            acc[3] += tx.sapling.outputs.len();
            acc[4] += tx.orchard.actions.len();
            acc
        });
        assert_eq!(standalone.len(), 44);
        assert_eq!(totals, [53, 35, 5, 12, 44]);

        assert!(matches!(decode_transaction(&[0u8; 8]), Err(DecodeError::Malformed(_))));
        let first = tx_spans(&raw)[0].clone();
        let mut padded = raw[first].to_vec();
        padded.push(0);
        assert!(matches!(decode_transaction(&padded), Err(DecodeError::Trailing(1))));
        let mut padded_block = raw.clone();
        padded_block.push(0);
        assert!(matches!(super::block(&padded_block), Err(DecodeError::Trailing(1))));
    }

    /// - Header prefix of each fixture → the full-block decode's hash + parent
    /// - Hash = SHA-256d of the header bytes (sha2, independent of librustzcash)
    #[test]
    fn a_header_links_to_its_block_hash_and_parent() {
        use sha2::{Digest, Sha256};
        for height in [419_200, 1_000_000, 1_687_104, 2_000_000, 2_500_000] {
            let raw = fixture(height);
            let mut cursor = Cursor::new(raw.as_slice());
            RawHeader::read(&mut cursor).expect("header");
            let header = &raw[..cursor.position() as usize];

            let decoded = block(&raw).expect("block decodes");
            let sha256d: [u8; 32] = Sha256::digest(Sha256::digest(header)).into();
            let prev: [u8; 32] = header[4..36].try_into().expect("32");
            let expected =
                BlockLink { hash: decoded.header().hash, prev_hash: decoded.header().prev_hash };
            assert_eq!(block_link(header).expect("header decodes"), expected, "height {height}");
            assert_eq!(
                (BlockHash::from(sha256d), BlockHash::from(prev)),
                (expected.hash, expected.prev_hash),
                "height {height}"
            );
            let padded = [header, &[0]].concat();
            assert!(matches!(block_link(&padded), Err(DecodeError::Trailing(1))));
        }
    }

    /// Header merkle root = Bitcoin merkle tree over txids (every tx version) → pins each txid,
    /// v4 (byte hash) and v5 (ZIP 244 digest) alike
    #[test]
    fn every_fixture_txid_rebuilds_its_header_merkle_root() {
        use sha2::{Digest, Sha256};
        let sha256d = |bytes: &[u8]| -> [u8; 32] { Sha256::digest(Sha256::digest(bytes)).into() };

        for height in [419_200, 1_000_000, 1_687_104, 2_000_000, 2_500_000] {
            let block = block(&fixture(height)).expect("block decodes");
            let mut level: Vec<[u8; 32]> =
                block.transactions().iter().map(|tx| <[u8; 32]>::from(tx.txid)).collect();
            while level.len() > 1 {
                if level.len() % 2 == 1 {
                    level.push(*level.last().expect("non-empty"));
                }
                level = level.chunks(2).map(|pair| sha256d(&pair.concat())).collect();
            }
            let merkle_root = <[u8; 32]>::from(block.header().merkle_root);
            assert_eq!(level[0], merkle_root, "merkle root at {height}");
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
        use zcash_protocol::value::Zatoshis as Zats;

        let joinsplit = |old: u64, new: u64| {
            JsDescription::from_parts(
                Zats::from_u64(old).expect("in range"),
                Zats::from_u64(new).expect("in range"),
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
        assert_eq!(i64::from(decoded.sprout.value_balance), 6_500);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// librustzcash-written NU6.3 txs (v5 + v6, Ironwood included) decode to their source
        #[test]
        fn nu6_3_transactions_decode_to_their_source(
            tx in zcash_primitives::transaction::testing::arb_tx(BranchId::Nu6_3)
        ) {
            let mut raw = Vec::new();
            tx.write(&mut raw).expect("writes");
            let decoded = decode_transaction(&raw).expect("decodes");

            let source_actions = |bundle: Option<&orchard::Bundle<_, ZatBalance>>| {
                let actions: Vec<_> = bundle
                    .map(|b| {
                        b.actions()
                            .iter()
                            .map(|a| (a.nullifier().to_bytes(), a.cmx().to_bytes()))
                            .collect()
                    })
                    .unwrap_or_default();
                (actions, bundle.map_or(0, |b| i64::from(*b.value_balance())))
            };
            let decoded_actions = |pool: &OrchardData| {
                let actions: Vec<_> = pool
                    .actions
                    .iter()
                    .map(|a| (<[u8; 32]>::from(a.nullifier), <[u8; 32]>::from(a.cmx)))
                    .collect();
                (actions, i64::from(pool.value_balance))
            };
            let sapling = tx.sapling_bundle();
            let vout: Vec<_> = decoded.transparent.outputs.iter().map(|o| u64::from(o.value)).collect();
            let source_vout: Vec<_> = tx
                .transparent_bundle()
                .map_or(vec![], |b| b.vout.iter().map(|o| o.value().into_u64()).collect());
            let spends: Vec<_> =
                decoded.sapling.spends.iter().map(|s| <[u8; 32]>::from(s.nullifier)).collect();
            let source_spends: Vec<_> =
                sapling.map_or(vec![], |b| b.shielded_spends().iter().map(|s| s.nullifier().0).collect());
            let cmus: Vec<_> = decoded.sapling.outputs.iter().map(|o| <[u8; 32]>::from(o.cmu)).collect();
            let source_cmus: Vec<_> = sapling
                .map_or(vec![], |b| b.shielded_outputs().iter().map(|o| o.cmu().to_bytes()).collect());
            let sapling_balance = sapling.map_or(0, |b| i64::from(*b.value_balance()));

            use proptest::prop_assert_eq;
            prop_assert_eq!(<[u8; 32]>::from(decoded.txid), *tx.txid().as_ref());
            prop_assert_eq!(vout, source_vout);
            prop_assert_eq!(spends, source_spends);
            prop_assert_eq!(cmus, source_cmus);
            prop_assert_eq!(i64::from(decoded.sapling.value_balance), sapling_balance);
            prop_assert_eq!(decoded_actions(&decoded.orchard), source_actions(tx.orchard_bundle()));
            prop_assert_eq!(decoded_actions(&decoded.ironwood), source_actions(tx.ironwood_bundle()));
        }
    }

    /// Push forms zcashd/zebra emit: `OP_n` for 1–16, minimal LE bytes above (sign byte at 128)
    #[test]
    fn bip34_heights_decode_in_every_push_form() {
        let cases: [(&[u8], Option<u32>); 8] = [
            (&[0x51], Some(1)),
            (&[0x60], Some(16)),
            (&[0x01, 0x11], Some(17)),
            (&[0x02, 0x80, 0x00], Some(128)),
            (&[0x03, 0x40, 0x42, 0x0f], Some(1_000_000)),
            (&[0x04, 0x2f, 0x4f, 0x34, 0x00], Some(3_428_143)),
            (&[0x03, 0x40, 0x42], None),
            (&[0x00], None),
        ];
        for (script, height) in cases {
            assert_eq!(bip34_height(script), height, "{script:02x?}");
        }
    }
}
