//! Golden on-disk encodings for every persisted type.
//!
//! # Why this module exists
//!
//! Every type here is written to LMDB. Its encoded bytes are a compatibility
//! contract with every database already on disk: change them and a running
//! node either mis-reads its own history or refuses to open it.
//!
//! Nothing in the type system enforces that contract. `DbCodec` is
//! hand-written per type, so an innocuous-looking edit — reordering two
//! fields, widening an integer, adding a variant — changes the encoding
//! silently. These tests are the thing that notices, and they are what makes
//! it safe to move this code between crates: a move that preserves every
//! golden preserves every existing database.
//!
//! # What a failure means
//!
//! A failure is not a bug in this module. It means the encoding changed, and
//! that every deployment will rebuild its database on its next start. Update the
//! golden only after accepting that cost, and bump the schema version with it.
//!
//! # What these goldens are not
//!
//! They pin *current* behaviour. They are not an independent derivation of
//! what the encoding ought to be, so they cannot catch a format that was
//! wrong from the start — as happened in #1313, where a golden was minted
//! little-endian for a field whose established on-disk format is big-endian,
//! enshrining the very bug it should have caught. Where an encoding's
//! correctness is separately known, assert it separately;
//! [`crate::types::db::block`] does exactly that for chainwork.
//!
//! # Coverage
//!
//! Every type implementing `DbCodec` is pinned here, except the
//! three `Persistent*` types in [`crate::types::db::block`],
//! which are module-private by design and whose bytes appear inside the
//! `block_header_data` golden — pinning them separately would mean widening
//! their visibility to test them.
//!
//! `MempoolInfo` is deliberately absent. It once carried an on-disk encoding
//! and was pinned here, but nothing ever wrote it: the mempool is live state,
//! rebuilt from the validator on every start. It is now
//! [`zaino_primitives::types::MempoolInfo`] with no encoding to pin.

use core::num::NonZeroU128;
use std::fmt::Debug;

use crate::codec::{DbCodec, FixedEncodedLen};
use crate::store::capability::{DbMetadata, DbVersion};
use crate::types::db::commitment::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes};
use crate::types::db::legacy::AddrEventBytes;
use crate::types::db::metadata::FinalisedTxOutSetInfoAccumulator;
use crate::types::EquihashSolution;
use crate::types::{
    AbsoluteChainWork, AddrHistRecord, AddrScript, BlockContext, BlockData, BlockHash,
    BlockHeaderData, CompactDifficulty, CompactOrchardAction, CompactSaplingOutput,
    CompactSaplingSpend, Height, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx,
    SaplingTxList, ScriptType, ShardIndex, ShardRoot, TransactionHash, TransparentCompactTx,
    TransparentTxList, TxInCompact, TxLocation, TxOutCompact, TxidList,
};

/// The chainwork the golden block context carries.
const CHAINWORK: NonZeroU128 = NonZeroU128::new(0x0dec_0de0).expect("nonzero literal");

/// A valid nBits value. Passes zebra's compact-difficulty validation without
/// corresponding to any real block.
const TEST_VALID_NBITS: u32 = 0x2007_ffff;

/* ─────────────────────────────── assertions ─────────────────────────────── */

/// Asserts `value` encodes to exactly `expected_hex`, and that those bytes
/// decode and re-encode to themselves.
///
/// The round trip is expressed as `encode(decode(bytes)) == bytes` rather than
/// `decode(encode(v)) == v` so it needs no `PartialEq` on the type — several
/// persisted types do not implement it — and because it is the stronger
/// statement anyway: it is the on-disk bytes, not the in-memory value, that
/// have to survive.
fn assert_golden<T: DbCodec>(name: &str, value: &T, expected_hex: &str) {
    let encoded = value
        .to_bytes()
        .unwrap_or_else(|e| panic!("{name}: encode failed: {e}"));
    assert_eq!(
        hex::encode(&encoded),
        expected_hex,
        "{name}: on-disk encoding drifted. If this change is intentional, every \
         deployment will rebuild its database: update the golden and bump the \
         schema version together."
    );

    let decoded = T::from_bytes(&encoded)
        .unwrap_or_else(|e| panic!("{name}: its own golden bytes failed to decode: {e}"));
    let re_encoded = decoded
        .to_bytes()
        .unwrap_or_else(|e| panic!("{name}: re-encode after decode failed: {e}"));
    assert_eq!(
        hex::encode(&re_encoded),
        expected_hex,
        "{name}: decode/encode is not the identity on its own bytes"
    );
}

/// Asserts the length `FixedEncodedLen` advertises matches what the encoder
/// actually produced.
///
/// These two are read separately: the encoder writes rows, and
/// `ENCODED_LEN` is what the readers use to stride across fixed-width
/// records. A disagreement misaligns every read after the first.
fn assert_fixed_len<T: DbCodec + FixedEncodedLen + Debug>(name: &str, value: &T) {
    let encoded = value
        .to_bytes()
        .unwrap_or_else(|e| panic!("{name}: encode failed: {e}"));
    let advertised = T::ENCODED_LEN;
    assert_eq!(
        advertised,
        encoded.len(),
        "{name}: FixedEncodedLen advertises {advertised} bytes but the encoder wrote {}",
        encoded.len()
    );
}

/* ─────────────────────────── canonical fixtures ─────────────────────────── */
//
// Each fixture uses a distinct repeated byte per field, so a transposed pair
// shows up in the golden as a recognisable run rather than as noise.

fn block_hash() -> BlockHash {
    BlockHash::from([0x11; 32])
}

fn transaction_hash() -> TransactionHash {
    TransactionHash::from([0x22; 32])
}

fn height() -> Height {
    Height(123_456)
}

fn shard_index() -> ShardIndex {
    ShardIndex(1_234)
}

fn addr_script() -> AddrScript {
    AddrScript::new([0x33; 20], ScriptType::P2SH as u8)
}

fn outpoint() -> Outpoint {
    Outpoint::new([0x44; 32], 7)
}

/// The regtest variant deliberately: it is the short one (36 bytes against
/// 1344), so the goldens that embed a solution stay readable.
fn equihash_solution() -> EquihashSolution {
    EquihashSolution::Regtest([0x55; 36])
}

fn block_data() -> BlockData {
    BlockData {
        version: 4,
        time: 0x6543_2100,
        merkle_root: [0x66; 32],
        block_commitments: [0x77; 32],
        bits: CompactDifficulty::try_from_bits(TEST_VALID_NBITS).expect("valid nBits"),
        nonce: [0x88; 32],
        solution: equihash_solution(),
    }
}

fn block_context() -> BlockContext<AbsoluteChainWork> {
    BlockContext::new(
        block_hash(),
        BlockHash::from([0x99; 32]),
        AbsoluteChainWork::new(CHAINWORK),
        height(),
    )
}

fn block_header_data() -> BlockHeaderData<AbsoluteChainWork> {
    BlockHeaderData::new(block_context(), block_data())
}

fn tx_in_compact() -> TxInCompact {
    TxInCompact::new([0xaa; 32], 3)
}

fn tx_out_compact() -> TxOutCompact {
    TxOutCompact::new(21_000_000, [0xbb; 20], ScriptType::P2PKH as u8)
        .expect("value within compact range")
}

fn transparent_compact_tx() -> TransparentCompactTx {
    TransparentCompactTx::new(vec![tx_in_compact()], vec![tx_out_compact()])
}

fn compact_sapling_spend() -> CompactSaplingSpend {
    CompactSaplingSpend::new([0xcc; 32])
}

fn compact_sapling_output() -> CompactSaplingOutput {
    CompactSaplingOutput::new([0xdd; 32], [0xde; 32], [0xdf; 52])
}

/// Negative value pool: the sign is carried on disk, and a widening or
/// unsigned-ing of that field would otherwise pass unnoticed.
fn sapling_compact_tx() -> SaplingCompactTx {
    SaplingCompactTx::new(
        Some(-4_200),
        vec![compact_sapling_spend()],
        vec![compact_sapling_output()],
    )
}

fn compact_orchard_action() -> CompactOrchardAction {
    CompactOrchardAction::new([0xe0; 32], [0xe1; 32], [0xe2; 32], [0xe3; 52])
}

fn orchard_compact_tx() -> OrchardCompactTx {
    OrchardCompactTx::new(Some(9_100), vec![compact_orchard_action()])
}

fn tx_location() -> TxLocation {
    TxLocation::new(123_456, 9)
}

fn addr_hist_record() -> AddrHistRecord {
    AddrHistRecord::new(tx_location(), 2, 555_000, AddrEventBytes::FLAG_MINED)
}

fn shard_root() -> ShardRoot {
    ShardRoot::new([0xf0; 32], [0xf1; 32], 123_456)
}

fn txid_list() -> TxidList {
    TxidList::new(vec![transaction_hash(), TransactionHash::from([0x23; 32])])
}

/// Includes a `None` slot. The list types encode absent per-tx entries, and
/// how absence is written is as much a part of the contract as presence.
fn transparent_tx_list() -> TransparentTxList {
    TransparentTxList::new(vec![Some(transparent_compact_tx()), None])
}

fn sapling_tx_list() -> SaplingTxList {
    SaplingTxList::new(vec![Some(sapling_compact_tx()), None])
}

fn orchard_tx_list() -> OrchardTxList {
    OrchardTxList::new(vec![Some(orchard_compact_tx()), None])
}

/// Ironwood present: the pool is optional on disk (rows exist only from
/// schema v1.3.0 / NU6.3), so `Some` is the case that pins its layout.
fn commitment_tree_roots() -> CommitmentTreeRoots {
    CommitmentTreeRoots::new([0x01; 32], [0x02; 32], Some([0x03; 32]))
}

fn commitment_tree_sizes() -> CommitmentTreeSizes {
    CommitmentTreeSizes::new(11, 22, 33)
}

fn commitment_tree_data() -> CommitmentTreeData {
    CommitmentTreeData::new(commitment_tree_roots(), commitment_tree_sizes())
}

fn txout_set_accumulator() -> FinalisedTxOutSetInfoAccumulator {
    FinalisedTxOutSetInfoAccumulator::new(101, 202, 303, [0x5a; 32], 404)
}

fn db_version() -> DbVersion {
    DbVersion::new(1, 3, 0)
}

fn db_metadata() -> DbMetadata {
    DbMetadata::new(db_version(), [0x7e; 32])
}

/* ──────────────────────────────── goldens ──────────────────────────────── */

#[test]
fn primitive_goldens() {
    assert_golden(
        "BlockHash",
        &block_hash(),
        "1111111111111111111111111111111111111111111111111111111111111111",
    );
    assert_golden(
        "TransactionHash",
        &transaction_hash(),
        "2222222222222222222222222222222222222222222222222222222222222222",
    );
    // Height is big-endian on purpose: heights are B-tree keys, and
    // lexicographic key order has to match numeric order.
    assert_golden("Height", &height(), "0001e240");
    // ShardIndex, same reason.
    assert_golden("ShardIndex", &shard_index(), "000004d2");
    assert_golden(
        "AddrScript",
        &addr_script(),
        "333333333333333333333333333333333333333301",
    );
    assert_golden(
        "Outpoint",
        &outpoint(),
        "444444444444444444444444444444444444444444444444444444444444444407000000",
    );
    assert_golden("ScriptType", &ScriptType::NonStandard, "ff");
    assert_golden("TxLocation", &tx_location(), "0001e2400009");
    assert_golden(
        "ShardRoot",
        &shard_root(),
        "f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f140e20100",
    );
}

#[test]
fn block_goldens() {
    assert_golden(
        "EquihashSolution",
        &equihash_solution(),
        "01555555555555555555555555555555555555555555555555555555555555555555555555",
    );
    assert_golden(
        "BlockData",
        &block_data(),
        "04000000002143650000000066666666666666666666666666666666666666666666666666666666666666667777777777777777777777777777777777777777777777777777777777777777ffff0720888888888888888888888888888888888888888888888888888888888888888801555555555555555555555555555555555555555555555555555555555555555555555555",
    );
    // The composite that reaches disk as the `headers` row: an outer V2 tag,
    // then `PersistentBlockContext` V2 (itself two `BlockHash`es, a
    // big-endian `AbsoluteChainWork` and a big-endian `Height`), then `BlockData` V1.
    // This is also what pins the three module-private `Persistent*` types.
    assert_golden(
        "BlockHeaderData",
        &block_header_data(),
        "11111111111111111111111111111111111111111111111111111111111111119999999999999999999999999999999999999999999999999999999999999999000000000000000000000000000000000000000000000000000000000dec0de00001e24004000000002143650000000066666666666666666666666666666666666666666666666666666666666666667777777777777777777777777777777777777777777777777777777777777777ffff0720888888888888888888888888888888888888888888888888888888888888888801555555555555555555555555555555555555555555555555555555555555555555555555",
    );
}

#[test]
fn transaction_goldens() {
    assert_golden(
        "TxInCompact",
        &tx_in_compact(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa03000000",
    );
    assert_golden(
        "TxOutCompact",
        &tx_out_compact(),
        "406f400100000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb00",
    );
    assert_golden(
        "TransparentCompactTx",
        &transparent_compact_tx(),
        "01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0300000001406f400100000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb00",
    );
    assert_golden(
        "CompactSaplingSpend",
        &compact_sapling_spend(),
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    );
    assert_golden(
        "CompactSaplingOutput",
        &compact_sapling_output(),
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddededededededededededededededededededededededededededededededededfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdf",
    );
    assert_golden(
        "SaplingCompactTx",
        &sapling_compact_tx(),
        "0198efffffffffffff01cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddededededededededededededededededededededededededededededededededfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdf",
    );
    assert_golden(
        "CompactOrchardAction",
        &compact_orchard_action(),
        "e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3",
    );
    assert_golden(
        "OrchardCompactTx",
        &orchard_compact_tx(),
        "018c2300000000000001e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3",
    );
}

#[test]
fn list_goldens() {
    assert_golden(
        "TxidList",
        &txid_list(),
        "0222222222222222222222222222222222222222222222222222222222222222222323232323232323232323232323232323232323232323232323232323232323",
    );
    assert_golden(
        "TransparentTxList",
        &transparent_tx_list(),
        "020101aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0300000001406f400100000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0000",
    );
    assert_golden(
        "SaplingTxList",
        &sapling_tx_list(),
        "02010198efffffffffffff01cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddededededededededededededededededededededededededededededededededfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdf00",
    );
    assert_golden(
        "OrchardTxList",
        &orchard_tx_list(),
        "0201018c2300000000000001e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e300",
    );
}

#[test]
fn address_history_goldens() {
    // `AddrHistRecord` and the packed `AddrEventBytes` it becomes, the LMDB
    // `DUP_FIXED` value, must encode to identical bytes.
    assert_golden(
        "AddrHistRecord",
        &addr_hist_record(),
        "0001e2400009000201f877080000000000",
    );
    assert_golden(
        "AddrEventBytes",
        &AddrEventBytes::from_record(&addr_hist_record()).expect("pack"),
        "0001e2400009000201f877080000000000",
    );
}

#[test]
fn commitment_tree_goldens() {
    assert_golden(
        "CommitmentTreeRoots",
        &commitment_tree_roots(),
        "01010101010101010101010101010101010101010101010101010101010101010202020202020202020202020202020202020202020202020202020202020202010303030303030303030303030303030303030303030303030303030303030303",
    );
    assert_golden(
        "CommitmentTreeSizes",
        &commitment_tree_sizes(),
        "0b0000001600000021000000",
    );
    assert_golden(
        "CommitmentTreeData",
        &commitment_tree_data(),
        "010101010101010101010101010101010101010101010101010101010101010102020202020202020202020202020202020202020202020202020202020202020103030303030303030303030303030303030303030303030303030303030303030b0000001600000021000000",
    );
}

#[test]
fn metadata_goldens() {
    assert_golden(
        "FinalisedTxOutSetInfoAccumulator",
        &txout_set_accumulator(),
        "6500000000000000ca000000000000002f010000000000005a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a9401000000000000",
    );
    assert_golden("DbVersion", &db_version(), "010000000300000000000000");
    assert_golden(
        "DbMetadata",
        &db_metadata(),
        "0100000003000000000000007e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e",
    );
}

/// Every fixed-width type must advertise the length it actually writes.
///
/// Fixed-width rows are read by striding, so an advertised length that
/// disagrees with the encoder misaligns every record after the first — a
/// failure that surfaces far from its cause.
#[test]
fn fixed_lengths_match_the_encoder() {
    assert_fixed_len("BlockHash", &block_hash());
    assert_fixed_len("TransactionHash", &transaction_hash());
    assert_fixed_len("Height", &height());
    assert_fixed_len("ShardIndex", &shard_index());
    assert_fixed_len("AddrScript", &addr_script());
    assert_fixed_len("Outpoint", &outpoint());
    assert_fixed_len("ScriptType", &ScriptType::NonStandard);
    assert_fixed_len("TxInCompact", &tx_in_compact());
    assert_fixed_len("TxOutCompact", &tx_out_compact());
    assert_fixed_len("CompactSaplingSpend", &compact_sapling_spend());
    assert_fixed_len("CompactSaplingOutput", &compact_sapling_output());
    assert_fixed_len("CompactOrchardAction", &compact_orchard_action());
    assert_fixed_len("TxLocation", &tx_location());
    assert_fixed_len("AddrHistRecord", &addr_hist_record());
    assert_fixed_len(
        "AddrEventBytes",
        &AddrEventBytes::from_record(&addr_hist_record()).expect("pack"),
    );
    assert_fixed_len("ShardRoot", &shard_root());
    assert_fixed_len("CommitmentTreeSizes", &commitment_tree_sizes());
    assert_fixed_len("FinalisedTxOutSetInfoAccumulator", &txout_set_accumulator());
    assert_fixed_len("DbVersion", &db_version());
    assert_fixed_len("DbMetadata", &db_metadata());
}

/// `DB_SCHEMA_V1_HASH` must be the BLAKE2b-256 of the schema text it claims
/// to summarise.
///
/// The constant is maintained by hand, from a shell recipe in a comment, and
/// is compared against `DbMetadata::schema_hash` on every open. Nothing else
/// checks that the two agree, so the drift detector could itself drift —
/// leaving a schema change that alters the text but not the hash to open
/// silently against a database built before it.
#[test]
fn schema_hash_matches_the_schema_text() {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;

    use crate::store::finalised_source::v1::{DB_SCHEMA_V1_HASH, DB_SCHEMA_V1_TEXT};

    let mut hasher = Blake2bVar::new(32).expect("32 is a valid blake2b output length");
    hasher.update(DB_SCHEMA_V1_TEXT.as_bytes());
    let mut computed = [0u8; 32];
    hasher
        .finalize_variable(&mut computed)
        .expect("output length matches");

    assert_eq!(
        hex::encode(computed),
        hex::encode(DB_SCHEMA_V1_HASH),
        "DB_SCHEMA_V1_HASH does not match blake2b-256(db_schema_v1.txt). \
         Either the schema text changed without the constant being regenerated \
         (`b2sum -l 256 db_schema_v1.txt`), or the constant was mistyped — in \
         both cases the on-open schema-drift check is not checking what it \
         claims to."
    );
}
