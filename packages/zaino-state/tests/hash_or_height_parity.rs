//! Zaino's `HashOrHeight` parser must accept and resolve exactly what zebra's does.
//!
//! RPC callers write block identifiers in zebra's syntax, so a string zebra
//! accepts and Zaino refuses, or the reverse, is a behaviour change on the wire.
//! The goldens in `zaino-primitives` pin the contract on their own; this test
//! pins it to zebra's parser for as long as zebra-state is in the build.

#![forbid(unsafe_code)]

#[path = "support/golden.rs"]
mod golden;

use std::path::PathBuf;

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

/// How one parser resolved one input, in the form the golden file stores.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct Outcome {
    input: &'static str,
    /// `None` for a plain parse, the tip for a relative parse.
    tip: Option<u32>,
    relative: bool,
    /// `None` when the parser refused the input.
    resolved: Option<String>,
}

/// Renders an identifier so that hashes and heights stay distinguishable in the golden.
fn render(identifier: HashOrHeight) -> String {
    match identifier {
        HashOrHeight::Hash(hash) => format!("hash:{}", hex::encode(<[u8; 32]>::from(hash))),
        HashOrHeight::Height(height) => format!("height:{}", u32::from(height)),
    }
}

/// A height the test itself knows is in range.
fn height(value: u32) -> Height {
    Height::try_from(value).expect("the test tip is in range")
}

/// Every input through both parse forms, resolved by `parse` and `parse_relative`.
fn outcomes(
    parse: impl Fn(&str) -> Option<HashOrHeight>,
    parse_relative: impl Fn(&str, Option<u32>) -> Option<HashOrHeight>,
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for input in INPUTS {
        outcomes.push(Outcome {
            input,
            tip: None,
            relative: false,
            resolved: parse(input).map(render),
        });
        for tip in [None, Some(TIP)] {
            outcomes.push(Outcome {
                input,
                tip,
                relative: true,
                resolved: parse_relative(input, tip).map(render),
            });
        }
    }
    outcomes
}

/// Zaino's parser's outcomes.
fn zaino_outcomes() -> Vec<Outcome> {
    outcomes(
        |input| input.parse().ok(),
        |input, tip| HashOrHeight::parse_relative(input, tip.map(height)).ok(),
    )
}

/// Zebra's identifier, in Zaino's type so the two compare directly.
fn translate(zebra: zebra_state::HashOrHeight) -> HashOrHeight {
    match zebra {
        zebra_state::HashOrHeight::Hash(hash) => HashOrHeight::Hash(BlockHash::from(hash.0)),
        zebra_state::HashOrHeight::Height(height) => HashOrHeight::Height(
            Height::try_from(height.0).expect("zebra only yields heights in range"),
        ),
    }
}

/// Zebra's parser's outcomes.
fn zebra_outcomes() -> Vec<Outcome> {
    outcomes(
        |input| {
            input
                .parse::<zebra_state::HashOrHeight>()
                .ok()
                .map(translate)
        },
        |input, tip| {
            zebra_state::HashOrHeight::new(input, tip.map(zebra_chain::block::Height))
                .ok()
                .map(translate)
        },
    )
}

#[test]
fn zaino_parses_block_identifiers_exactly_as_zebra_does() {
    assert_eq!(zaino_outcomes(), zebra_outcomes());
    golden::assert_golden(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures"),
        "hash_or_height",
        &zebra_outcomes(),
    );
}
