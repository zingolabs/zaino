//! A block lookup key: a hash or a height, parsed from one RPC string.

use core::fmt;
use core::str::FromStr;

use crate::types::{BlockHash, Height};

/// A block lookup key naming a block by hash or by height, whose `FromStr`
/// reproduces the parse Zebra applies to RPC `hash_or_height` strings — a
/// 64-character display-order hex hash first, else a decimal height bounded
/// by [`Height`]'s `2^31 - 1` limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashOrHeight {
    /// The block with this hash.
    Hash(BlockHash),
    /// The block at this height.
    Height(Height),
}

/// Error returned when a string is neither a block hash nor a height, whose
/// display text reproduces Zebra's verbatim because it reaches RPC callers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("parse error: could not convert the input string to a hash or height")]
pub struct HashOrHeightParseError {
    /// The string that was rejected.
    pub input: String,
}

impl HashOrHeight {
    /// Parses like `FromStr`, additionally resolving a negative height
    /// against `tip` the way Zebra's `getblock` does (`-1` names the tip).
    pub fn parse_with_tip(s: &str, tip: Option<Height>) -> Result<Self, HashOrHeightParseError> {
        let reject = || HashOrHeightParseError {
            input: s.to_string(),
        };
        if let Ok(parsed) = s.parse() {
            return Ok(parsed);
        }
        let diff = s.parse::<i64>().map_err(|_| reject())?;
        if diff >= 0 {
            return Err(reject());
        }
        let tip = i64::from(u32::from(tip.ok_or_else(reject)?));
        // Zebra checks `tip + diff` is itself a valid height before the +1
        // correction, so "-1" at a genesis tip is rejected, not Height(0).
        let stepped = tip.checked_add(diff).ok_or_else(reject)?;
        let stepped = u32::try_from(stepped)
            .ok()
            .and_then(|h| Height::try_from(h).ok())
            .ok_or_else(reject)?;
        stepped.checked_add(1).map(Self::Height).ok_or_else(reject)
    }
}

impl FromStr for HashOrHeight {
    type Err = HashOrHeightParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(hash) = parse_display_hex_hash(s) {
            return Ok(Self::Hash(hash));
        }
        s.parse::<u32>()
            .ok()
            .and_then(|h| Height::try_from(h).ok())
            .map(Self::Height)
            .ok_or_else(|| HashOrHeightParseError {
                input: s.to_string(),
            })
    }
}

impl fmt::Display for HashOrHeight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hash(hash) => hash.fmt(f),
            Self::Height(height) => height.fmt(f),
        }
    }
}

/// Parses a 64-character display-order hex string into an internal-order hash.
fn parse_display_hex_hash(s: &str) -> Option<BlockHash> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let byte = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        // Display order is byte-reversed relative to internal order.
        hash[31 - i] = byte;
    }
    Some(BlockHash::from(hash))
}

/// Decodes one hex digit, in either case.
fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mainnet genesis hash in display order.
    const GENESIS_DISPLAY: &str =
        "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08";

    fn parsed(s: &str) -> HashOrHeight {
        s.parse().expect("input must parse")
    }

    #[test]
    fn plain_heights_parse() {
        assert_eq!(
            parsed("0"),
            HashOrHeight::Height(Height::try_from(0).expect("valid"))
        );
        assert_eq!(
            parsed("1687104"),
            HashOrHeight::Height(Height::try_from(1_687_104).expect("valid"))
        );
    }

    #[test]
    fn height_bound_is_u32_max_halved() {
        let max = (1u32 << 31) - 1;
        assert_eq!(
            parsed("2147483647"),
            HashOrHeight::Height(Height::try_from(max).expect("valid"))
        );
        assert!("2147483648".parse::<HashOrHeight>().is_err());
        assert!("4294967295".parse::<HashOrHeight>().is_err());
        assert!("4294967296".parse::<HashOrHeight>().is_err());
    }

    #[test]
    fn u32_parse_quirks_are_wire_behavior() {
        // Rust's u32 parsing accepts a leading '+' and leading zeros.
        assert_eq!(
            parsed("+5"),
            HashOrHeight::Height(Height::try_from(5).expect("valid"))
        );
        assert_eq!(
            parsed("007"),
            HashOrHeight::Height(Height::try_from(7).expect("valid"))
        );
        for rejected in ["-1", " 1", "1 ", "0x10", "1_000"] {
            assert!(
                rejected.parse::<HashOrHeight>().is_err(),
                "{rejected:?} must be rejected"
            );
        }
    }

    #[test]
    fn display_hex_hash_parses_byte_reversed() {
        let HashOrHeight::Hash(hash) = parsed(GENESIS_DISPLAY) else {
            panic!("genesis display hex must parse as a hash");
        };
        let bytes = <[u8; 32]>::from(hash);
        // Internal order: the display string's first byte lands last.
        assert_eq!(bytes[31], 0x00);
        assert_eq!(bytes[30], 0x04);
        assert_eq!(bytes[0], 0x08);
        // Display round-trips through the BlockHash Display impl.
        assert_eq!(hash.to_string(), GENESIS_DISPLAY);
    }

    #[test]
    fn hash_parse_accepts_either_case() {
        let upper = GENESIS_DISPLAY.to_uppercase();
        assert_eq!(parsed(&upper), parsed(GENESIS_DISPLAY));
    }

    #[test]
    fn sixty_four_decimal_digits_are_a_hash_never_a_height() {
        let input = "1".repeat(64);
        assert!(matches!(parsed(&input), HashOrHeight::Hash(_)));
    }

    #[test]
    fn near_miss_hashes_are_rejected() {
        let one_short = &GENESIS_DISPLAY[1..];
        let one_long = format!("0{GENESIS_DISPLAY}");
        let bad_first = format!("g{}", &GENESIS_DISPLAY[1..]);
        let bad_last = format!("{}g", &GENESIS_DISPLAY[..63]);
        for rejected in [
            one_short, &one_long, &bad_first, &bad_last, "", " ", "deadbeef",
        ] {
            assert!(
                rejected.parse::<HashOrHeight>().is_err(),
                "{rejected:?} must be rejected"
            );
        }
    }

    #[test]
    fn negative_heights_resolve_against_the_tip() {
        let tip = Height::try_from(100).expect("valid");
        let at = |s: &str| HashOrHeight::parse_with_tip(s, Some(tip));
        let height = |h: u32| HashOrHeight::Height(Height::try_from(h).expect("valid"));
        assert_eq!(at("-1").expect("tip"), height(100));
        assert_eq!(at("-2").expect("tip minus one"), height(99));
        // The reachable range is [1, tip]: the pre-correction underflow
        // check makes genesis unreachable by negative indexing.
        assert_eq!(at("-100").expect("lowest reachable"), height(1));
        assert!(at("-101").is_err(), "genesis is unreachable");
        assert!(at("-102").is_err(), "past genesis must be rejected");
        // Absolute forms pass through unchanged.
        assert_eq!(at("7").expect("absolute"), height(7));
        assert!(at("-0").is_err(), "minus zero is not a negative height");
    }

    #[test]
    fn negative_height_at_genesis_tip_is_rejected() {
        // Zebra checks `tip + diff` before the +1 correction, so "-1" at a
        // genesis tip underflows and is rejected rather than naming the tip.
        assert!(HashOrHeight::parse_with_tip("-1", Some(Height::GENESIS)).is_err());
    }

    #[test]
    fn negative_height_without_a_tip_is_rejected() {
        assert!(HashOrHeight::parse_with_tip("-1", None).is_err());
    }

    #[test]
    fn error_text_matches_zebra_verbatim() {
        let error = "deadbeef"
            .parse::<HashOrHeight>()
            .expect_err("must be rejected");
        assert_eq!(
            error.to_string(),
            "parse error: could not convert the input string to a hash or height"
        );
    }

    #[test]
    fn display_round_trips_both_variants() {
        for input in [GENESIS_DISPLAY, "1687104"] {
            assert_eq!(parsed(input).to_string(), input);
        }
    }
}
