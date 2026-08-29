//! Differential test pinning zaino's `HashOrHeight` parser to Zebra's.

use std::str::FromStr;

use zaino_primitives::types::HashOrHeight;

/// HYPOTHESIS: for every input string, `zaino_primitives`' `HashOrHeight`
/// parser accepts exactly when `zebra_state`'s does, and produces the same
/// variant and value.
#[test]
fn zaino_parser_agrees_with_zebra_oracle() {
    let inputs: &[&str] = &[
        // Plain heights, including both sides of Height::MAX = u32::MAX / 2
        // and both sides of the u32 range.
        "0",
        "1",
        "1687104",
        "2147483647",
        "2147483648",
        "4294967295",
        "4294967296",
        // Rust u32-parse quirks that are wire behavior.
        "+5",
        "007",
        "-1",
        " 1",
        "1 ",
        "0x10",
        "1_000",
        // Hashes: mainnet genesis in display order, upper case, mixed case.
        "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08",
        "00040FE8EC8471911BAA1DB1266EA15DD06B4A8A5C453883C000B031973DCE08",
        "00040fe8EC8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dcE08",
        // 64 decimal digits: hex-valid, so a hash, never a height.
        "1111111111111111111111111111111111111111111111111111111111111111",
        // Near-misses: 63 and 65 characters, one non-hex character at each end.
        "0040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08",
        "000040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08",
        "g0040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce0",
        "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dceg",
        "",
        " ",
        "deadbeef",
    ];
    for input in inputs {
        assert_agreement(
            input,
            HashOrHeight::from_str(input),
            zebra_state::HashOrHeight::from_str(input).map_err(|e| e.to_string()),
        );
    }
}

/// HYPOTHESIS: for every input string and tip, `parse_with_tip` agrees with
/// Zebra's tip-relative `HashOrHeight::new` on acceptance, variant, and
/// value.
#[test]
fn zaino_tip_relative_parse_agrees_with_zebra_oracle() {
    let inputs: &[&str] = &[
        // Tip-relative negative heights: tip, tip-1, exact underflow
        // boundary at genesis, one past it, and extremes.
        "-1",
        "-2",
        "-101",
        "-102",
        "-2147483648",
        "-9223372036854775808",
        // Still-absolute forms must behave as in the plain parse.
        "0",
        "100",
        "2147483648",
        "+5",
        "-0",
        "deadbeef",
        "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08",
    ];
    let tips: &[Option<u32>] = &[Some(100), Some(0), Some((1 << 31) - 1), None];
    for input in inputs {
        for tip in tips {
            let zaino_tip =
                tip.map(|t| zaino_primitives::types::Height::try_from(t).expect("valid tip"));
            let zebra_tip = tip.map(zebra_chain::block::Height);
            assert_agreement(
                &format!("{input:?} at tip {tip:?}"),
                HashOrHeight::parse_with_tip(input, zaino_tip),
                zebra_state::HashOrHeight::new(input, zebra_tip),
            );
        }
    }
}

/// Panics unless the two parse outcomes agree on acceptance, variant, and value.
fn assert_agreement<ZainoError: std::fmt::Debug, ZebraError: std::fmt::Debug>(
    input: &str,
    zaino: Result<HashOrHeight, ZainoError>,
    zebra: Result<zebra_state::HashOrHeight, ZebraError>,
) {
    match (zaino, zebra) {
        (Ok(HashOrHeight::Height(height)), Ok(zebra_state::HashOrHeight::Height(oracle))) => {
            assert_eq!(
                u32::from(height),
                oracle.0,
                "height value diverged on {input}"
            );
        }
        (Ok(HashOrHeight::Hash(hash)), Ok(zebra_state::HashOrHeight::Hash(oracle))) => {
            assert_eq!(
                <[u8; 32]>::from(hash),
                oracle.0,
                "hash bytes diverged on {input}"
            );
        }
        (Err(_), Err(_)) => {}
        (zaino, zebra) => {
            panic!("parsers diverged on {input}: zaino={zaino:?} zebra={zebra:?}")
        }
    }
}
