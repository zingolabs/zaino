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
//! golden only after accepting that cost; the pinned schema hash will change
//! with it, because the hash covers every canonical encoding.
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

use std::fmt::Debug;

use crate::codec::{DbCodec, FixedEncodedLen};
use crate::store::finalised_source::v1::schema;
use crate::store::finalised_source::v1::schema::canonical;
use crate::types::{ScriptType, ShardIndex, ShardRoot};

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
         deployment will rebuild its database: update this golden and the \
         schema hash golden together."
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

/* ────────────────────────── out-of-schema fixtures ────────────────────────── */
//
// Every type the finalised store persists takes its fixture from
// `schema::canonical`, which also feeds the schema hash. The shard types have
// an encoding but no table, so their fixtures live here.

/// A shard index fixture.
fn shard_index() -> ShardIndex {
    ShardIndex(1_234)
}

/// A shard root fixture with a distinct repeated byte per hash field.
fn shard_root() -> ShardRoot {
    ShardRoot::new([0xf0; 32], [0xf1; 32], 123_456)
}

/* ──────────────────────────────── goldens ──────────────────────────────── */

#[test]
fn primitive_goldens() {
    assert_golden(
        "BlockHash",
        &canonical::block_hash(),
        "1111111111111111111111111111111111111111111111111111111111111111",
    );
    assert_golden(
        "TransactionHash",
        &canonical::transaction_hash(),
        "2222222222222222222222222222222222222222222222222222222222222222",
    );
    // Height is big-endian on purpose: heights are B-tree keys, and
    // lexicographic key order has to match numeric order.
    assert_golden("Height", &canonical::height(), "0001e240");
    // ShardIndex, same reason.
    assert_golden("ShardIndex", &shard_index(), "000004d2");
    assert_golden(
        "AddrScript",
        &canonical::addr_script(),
        "333333333333333333333333333333333333333301",
    );
    assert_golden(
        "Outpoint",
        &canonical::outpoint(),
        "444444444444444444444444444444444444444444444444444444444444444407000000",
    );
    assert_golden("ScriptType", &ScriptType::NonStandard, "ff");
    assert_golden("TxLocation", &canonical::tx_location(), "0001e2400009");
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
        &canonical::equihash_solution(),
        "01555555555555555555555555555555555555555555555555555555555555555555555555",
    );
    assert_golden(
        "BlockData",
        &canonical::block_data(),
        "04000000002143650000000066666666666666666666666666666666666666666666666666666666666666667777777777777777777777777777777777777777777777777777777777777777ffff0720888888888888888888888888888888888888888888888888888888888888888801555555555555555555555555555555555555555555555555555555555555555555555555",
    );
    // The composite that reaches disk as the `headers` row: an outer V2 tag,
    // then `PersistentBlockContext` V2 (itself two `BlockHash`es, a
    // big-endian `AbsoluteChainWork` and a big-endian `Height`), then `BlockData` V1.
    // This is also what pins the three module-private `Persistent*` types.
    assert_golden(
        "BlockHeaderData",
        &canonical::block_header_data(),
        "11111111111111111111111111111111111111111111111111111111111111119999999999999999999999999999999999999999999999999999999999999999000000000000000000000000000000000000000000000000000000000dec0de00001e24004000000002143650000000066666666666666666666666666666666666666666666666666666666666666667777777777777777777777777777777777777777777777777777777777777777ffff0720888888888888888888888888888888888888888888888888888888888888888801555555555555555555555555555555555555555555555555555555555555555555555555",
    );
}

#[test]
fn transaction_goldens() {
    assert_golden(
        "TxInCompact",
        &canonical::tx_in_compact(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa03000000",
    );
    assert_golden(
        "TxOutCompact",
        &canonical::tx_out_compact(),
        "406f400100000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb00",
    );
    assert_golden(
        "TransparentCompactTx",
        &canonical::transparent_compact_tx(),
        "01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0300000001406f400100000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb00",
    );
    assert_golden(
        "CompactSaplingSpend",
        &canonical::compact_sapling_spend(),
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    );
    assert_golden(
        "CompactSaplingOutput",
        &canonical::compact_sapling_output(),
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddededededededededededededededededededededededededededededededededfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdf",
    );
    assert_golden(
        "SaplingCompactTx",
        &canonical::sapling_compact_tx(),
        "0198efffffffffffff01cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddededededededededededededededededededededededededededededededededfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdf",
    );
    assert_golden(
        "CompactOrchardAction",
        &canonical::compact_orchard_action(),
        "e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3",
    );
    assert_golden(
        "OrchardCompactTx",
        &canonical::orchard_compact_tx(),
        "018c2300000000000001e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3",
    );
}

#[test]
fn list_goldens() {
    assert_golden(
        "TxidList",
        &canonical::txid_list(),
        "0222222222222222222222222222222222222222222222222222222222222222222323232323232323232323232323232323232323232323232323232323232323",
    );
    assert_golden(
        "TransparentTxList",
        &canonical::transparent_tx_list(),
        "020101aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0300000001406f400100000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0000",
    );
    assert_golden(
        "SaplingTxList",
        &canonical::sapling_tx_list(),
        "02010198efffffffffffff01cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddededededededededededededededededededededededededededededededededfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdf00",
    );
    assert_golden(
        "OrchardTxList",
        &canonical::orchard_tx_list(),
        "0201018c2300000000000001e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e300",
    );
}

#[test]
fn address_history_goldens() {
    // `AddrHistRecord` and the packed `AddrEventBytes` it becomes, the LMDB
    // `DUP_FIXED` value, must encode to identical bytes.
    assert_golden(
        "AddrHistRecord",
        &canonical::addr_hist_record(),
        "0001e2400009000201f877080000000000",
    );
    assert_golden(
        "AddrEventBytes",
        &canonical::addr_event_bytes().expect("pack"),
        "0001e2400009000201f877080000000000",
    );
}

#[test]
fn commitment_tree_goldens() {
    assert_golden(
        "CommitmentTreeRoots",
        &canonical::commitment_tree_roots(),
        "01010101010101010101010101010101010101010101010101010101010101010202020202020202020202020202020202020202020202020202020202020202010303030303030303030303030303030303030303030303030303030303030303",
    );
    assert_golden(
        "CommitmentTreeSizes",
        &canonical::commitment_tree_sizes(),
        "0b0000001600000021000000",
    );
    assert_golden(
        "CommitmentTreeData",
        &canonical::commitment_tree_data(),
        "010101010101010101010101010101010101010101010101010101010101010102020202020202020202020202020202020202020202020202020202020202020103030303030303030303030303030303030303030303030303030303030303030b0000001600000021000000",
    );
}

#[test]
fn metadata_goldens() {
    assert_golden(
        "FinalisedTxOutSetInfoAccumulator",
        &canonical::txout_set_accumulator(),
        "6500000000000000ca000000000000002f010000000000005a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a9401000000000000",
    );
    assert_golden(
        "DbMetadata",
        &canonical::db_metadata(),
        "7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e",
    );
}

/// Every fixed-width type must advertise the length it actually writes.
///
/// Fixed-width rows are read by striding, so an advertised length that
/// disagrees with the encoder misaligns every record after the first — a
/// failure that surfaces far from its cause.
#[test]
fn fixed_lengths_match_the_encoder() {
    assert_fixed_len("BlockHash", &canonical::block_hash());
    assert_fixed_len("TransactionHash", &canonical::transaction_hash());
    assert_fixed_len("Height", &canonical::height());
    assert_fixed_len("ShardIndex", &shard_index());
    assert_fixed_len("AddrScript", &canonical::addr_script());
    assert_fixed_len("Outpoint", &canonical::outpoint());
    assert_fixed_len("ScriptType", &ScriptType::NonStandard);
    assert_fixed_len("TxInCompact", &canonical::tx_in_compact());
    assert_fixed_len("TxOutCompact", &canonical::tx_out_compact());
    assert_fixed_len("CompactSaplingSpend", &canonical::compact_sapling_spend());
    assert_fixed_len("CompactSaplingOutput", &canonical::compact_sapling_output());
    assert_fixed_len("CompactOrchardAction", &canonical::compact_orchard_action());
    assert_fixed_len("TxLocation", &canonical::tx_location());
    assert_fixed_len("AddrHistRecord", &canonical::addr_hist_record());
    assert_fixed_len(
        "AddrEventBytes",
        &canonical::addr_event_bytes().expect("pack"),
    );
    assert_fixed_len("ShardRoot", &shard_root());
    assert_fixed_len("CommitmentTreeSizes", &canonical::commitment_tree_sizes());
    assert_fixed_len(
        "FinalisedTxOutSetInfoAccumulator",
        &canonical::txout_set_accumulator(),
    );
    assert_fixed_len("DbMetadata", &canonical::db_metadata());
}

/// The schema hash of a build without the address-history index.
#[cfg(not(feature = "transparent_address_history_experimental"))]
const SCHEMA_HASH: &str = "b5c3a7e540c79599b68a99e243f9916426be44d6df69d9c9162350a153ccb581";

/// The schema hash of a build with the address-history index.
#[cfg(feature = "transparent_address_history_experimental")]
const SCHEMA_HASH: &str = "8518ecd83b75eb8258ac742016fc925d045acb18e39cd15382dfe11d477df9bd";

/// The computed schema hash changes exactly when a deployment must rebuild, so pinning it makes every rebuild a reviewed decision.
#[test]
fn schema_hash_golden() {
    let computed = schema::schema_hash().expect("every canonical record encodes");
    assert_eq!(
        hex::encode(computed),
        SCHEMA_HASH,
        "the finalised store schema hash changed. Every deployment will rebuild \
         its database on its next start: update this golden only after accepting \
         that cost."
    );
}
