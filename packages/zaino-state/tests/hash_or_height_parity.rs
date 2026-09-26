//! Zaino's `HashOrHeight` parser must accept and resolve exactly what zebra's does.
//!
//! RPC callers write block identifiers in zebra's syntax, so a string zebra
//! accepts and Zaino refuses, or the reverse, is a behaviour change on the wire.
//! The goldens in `zaino-primitives` pin the contract on their own; this test
//! pins it to zebra's parser for as long as zebra-state is in the build.

use zaino_primitives::types::{BlockHash, HashOrHeight, Height};

/// Inputs covering hashes, heights, u32-parse quirks, the protocol maximum, and negative heights.
const INPUTS: &[&str] = &[
    "ab00000000000000000000000000000000000000000000000000000000000001",
    "AB00000000000000000000000000000000000000000000000000000000000001",
    "1111111111111111111111111111111111111111111111111111111111111111",
    "ab0000000000000000000000000000000000000000000000000000000000001",
    "zz00000000000000000000000000000000000000000000000000000000000001",
    "0",
    "5",
    "+5",
    "007",
    "2147483647",
    "2147483648",
    "4294967295",
    "4294967296",
    "-0",
    "-1",
    "-2",
    "-99",
    "-100",
    "-101",
    "-9223372036854775808",
    " 1",
    "1 ",
    "0x10",
    "1.0",
    "",
];

/// The tip every negative height counts back from.
const TIP: u32 = 100;

/// Zebra's identifier, in Zaino's type so the two compare directly.
fn translate(zebra: zebra_state::HashOrHeight) -> HashOrHeight {
    match zebra {
        zebra_state::HashOrHeight::Hash(hash) => HashOrHeight::Hash(BlockHash::from(hash.0)),
        zebra_state::HashOrHeight::Height(height) => HashOrHeight::Height(
            Height::try_from(height.0).expect("zebra only yields heights in range"),
        ),
    }
}

/// A height the test itself knows is in range.
fn height(value: u32) -> Height {
    Height::try_from(value).expect("the test tip is in range")
}

#[test]
fn zaino_parses_block_identifiers_exactly_as_zebra_does() {
    for input in INPUTS {
        let zaino: Option<HashOrHeight> = input.parse().ok();
        let zebra = input
            .parse::<zebra_state::HashOrHeight>()
            .ok()
            .map(translate);
        assert_eq!(zaino, zebra, "from_str disagrees on {input:?}");

        for tip in [None, Some(TIP)] {
            let zaino = HashOrHeight::parse_relative(input, tip.map(height)).ok();
            let zebra = zebra_state::HashOrHeight::new(input, tip.map(zebra_chain::block::Height))
                .ok()
                .map(translate);
            assert_eq!(
                zaino, zebra,
                "relative parse disagrees on {input:?} with tip {tip:?}"
            );
        }
    }
}
