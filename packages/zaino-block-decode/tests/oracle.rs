//! The decoder against zebra-chain: the same bytes decoded both ways must give
//! the same block, field for field, on every transaction version the chain
//! has carried.
//!
//! The fixtures are mainnet blocks as `getblock <h> 0` returns them, chosen
//! for the formats they carry.

use zaino_block_decode::{DecodeError, decode_block, decode_transaction};
use zaino_primitives::types::{Block, ChainMetadata};
use zebra_chain::serialization::{ZcashDeserializeInto, ZcashSerialize};

/// Height, and what the block exercises.
const FIXTURES: &[(u32, &str)] = &[
    (1, "v1, a coinbase alone"),
    (250_000, "v1 and v2, JoinSplits with PHGR proofs"),
    (400_000, "v3 Overwinter"),
    (419_200, "v4 at Sapling activation"),
    (1_000_000, "v4 with Sapling spends and outputs"),
    (1_687_104, "v4 in the sandblast era, output-heavy"),
    (2_000_000, "v5 with Orchard actions"),
    (2_500_000, "v5 mixed pools"),
    (3_490_000, "v6 with Orchard and Ironwood bundles"),
];

fn fixture(height: u32) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/block_{height}.hex",
        env!("CARGO_MANIFEST_DIR")
    );
    let hex = std::fs::read_to_string(&path).expect("fixture readable");
    hex::decode(hex.trim()).expect("fixture is hex")
}

fn through_zebra(raw: &[u8]) -> Block {
    let zebra: zebra_chain::block::Block = raw
        .zcash_deserialize_into()
        .expect("zebra deserialises the fixture");
    zaino_convert_zebra::block_from_zebra(&zebra, ChainMetadata::ZERO)
        .expect("zebra block converts")
}

#[test]
fn every_fixture_decodes_to_what_zebra_chain_gives() {
    for &(height, what) in FIXTURES {
        let raw = fixture(height);
        let ours = decode_block(&raw, ChainMetadata::ZERO)
            .unwrap_or_else(|e| panic!("height {height} ({what}): {e}"));
        let theirs = through_zebra(&raw);

        assert_eq!(ours.header, theirs.header, "header at {height} ({what})");
        assert_eq!(
            ours.transactions.len(),
            theirs.transactions.len(),
            "transaction count at {height} ({what})"
        );
        for (index, (a, b)) in ours
            .transactions
            .iter()
            .zip(&theirs.transactions)
            .enumerate()
        {
            assert_eq!(a, b, "transaction {index} at {height} ({what})");
        }
        assert_eq!(ours, theirs, "block at {height} ({what})");
    }
}

/// A transaction cut out of a block decodes on its own to the same thing it
/// decodes to inside the block.
#[test]
fn a_transaction_decodes_alone_as_it_does_in_its_block() {
    for &(height, what) in FIXTURES {
        let raw = fixture(height);
        let zebra: zebra_chain::block::Block = raw
            .zcash_deserialize_into()
            .expect("zebra deserialises the fixture");
        let block = decode_block(&raw, ChainMetadata::ZERO).expect("block decodes");
        for (index, (tx, mined)) in zebra
            .transactions
            .iter()
            .zip(&block.transactions)
            .enumerate()
        {
            let bytes = tx.zcash_serialize_to_vec().expect("zebra serialises");
            let alone = decode_transaction(&bytes)
                .unwrap_or_else(|e| panic!("tx {index} at {height} ({what}): {e}"));
            assert_eq!(&alone, mined, "tx {index} at {height} ({what})");
        }
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut raw = fixture(1);
    raw.push(0);
    assert!(matches!(
        decode_block(&raw, ChainMetadata::ZERO),
        Err(DecodeError::Trailing(1))
    ));
}

#[test]
fn a_cut_block_is_truncated_not_misread() {
    let raw = fixture(2_000_000);
    for cut in [raw.len() / 3, raw.len() / 2, raw.len() - 1] {
        assert!(
            matches!(
                decode_block(&raw[..cut], ChainMetadata::ZERO),
                Err(DecodeError::Truncated { .. })
            ),
            "cut at {cut}"
        );
    }
}
