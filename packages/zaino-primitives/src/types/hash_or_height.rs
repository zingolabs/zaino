//! A block named by its hash or by its height, as an RPC caller writes it.

use core::{fmt, str::FromStr};

use super::{BlockHash, Height};

/// A block identified by its hash or by its height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashOrHeight {
    /// A block identified by its hash.
    Hash(BlockHash),
    /// A block identified by its height.
    Height(Height),
}

/// Why a string names no block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashOrHeightParseError {
    /// The string is neither a 64-digit hex hash nor a height in range.
    #[error("could not convert {0:?} to a block hash or height")]
    Unparseable(String),
    /// A negative height counts back from the tip, and no tip was given.
    #[error("a negative height needs a chain tip to count back from")]
    MissingTip,
    /// A negative height counts back past genesis.
    #[error("height {offset} counts back past genesis from tip {tip}")]
    BeforeGenesis {
        /// The negative height that was asked for.
        offset: i64,
        /// The tip it counted back from.
        tip: Height,
    },
}

impl HashOrHeight {
    /// Parses a hash, a height, or a negative height counted back from `tip`, where `-1` names the tip and `-n` names `tip - n + 1` for `n <= tip`.
    pub fn parse_relative(s: &str, tip: Option<Height>) -> Result<Self, HashOrHeightParseError> {
        if let Ok(parsed) = s.parse() {
            return Ok(parsed);
        }
        let offset: i64 = s
            .parse()
            .ok()
            .filter(|offset: &i64| offset.is_negative())
            .ok_or_else(|| HashOrHeightParseError::Unparseable(s.to_string()))?;
        let tip = tip.ok_or(HashOrHeightParseError::MissingTip)?;
        let counted_back = u32::try_from(offset.unsigned_abs())
            .ok()
            .and_then(|distance| tip.checked_sub(distance))
            .and_then(|below| below.checked_add(1))
            .ok_or(HashOrHeightParseError::BeforeGenesis { offset, tip })?;
        Ok(Self::Height(counted_back))
    }
}

impl FromStr for HashOrHeight {
    type Err = HashOrHeightParseError;

    /// Parses a 64-digit hex hash in display order first, then a decimal height no greater than `2^31 - 1`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(hash) = parse_display_hash(s) {
            return Ok(Self::Hash(hash));
        }
        s.parse::<u32>()
            .ok()
            .and_then(|height| Height::try_from(height).ok())
            .map(Self::Height)
            .ok_or_else(|| HashOrHeightParseError::Unparseable(s.to_string()))
    }
}

impl fmt::Display for HashOrHeight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hash(hash) => write!(f, "{hash}"),
            Self::Height(height) => write!(f, "{}", u32::from(*height)),
        }
    }
}

impl From<BlockHash> for HashOrHeight {
    fn from(hash: BlockHash) -> Self {
        Self::Hash(hash)
    }
}

impl From<Height> for HashOrHeight {
    fn from(height: Height) -> Self {
        Self::Height(height)
    }
}

/// Decodes 64 hex digits of either case, written in display order, into the hash's internal byte order.
fn parse_display_hash(s: &str) -> Option<BlockHash> {
    let digits = s.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (slot, pair) in bytes.iter_mut().rev().zip(digits.chunks_exact(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *slot = u8::try_from(high << 4 | low).ok()?;
    }
    Some(BlockHash::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(value: u32) -> Height {
        Height::try_from(value).expect("a test height is in range")
    }

    /// A display-order hash whose internal first byte is `0x01` and last byte is `0xab`.
    const DISPLAY_HASH: &str = "ab00000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn a_display_hash_parses_in_internal_byte_order() {
        let mut internal = [0u8; 32];
        internal[0] = 0x01;
        internal[31] = 0xab;
        assert_eq!(
            DISPLAY_HASH.parse::<HashOrHeight>(),
            Ok(HashOrHeight::Hash(BlockHash::from(internal)))
        );
        assert_eq!(
            DISPLAY_HASH.to_uppercase().parse::<HashOrHeight>(),
            Ok(HashOrHeight::Hash(BlockHash::from(internal)))
        );
    }

    #[test]
    fn a_hash_is_tried_before_a_height() {
        let all_digits = "1".repeat(64);
        assert!(matches!(
            all_digits.parse::<HashOrHeight>(),
            Ok(HashOrHeight::Hash(_))
        ));
    }

    #[test]
    fn heights_follow_u32_parsing_up_to_the_protocol_maximum() {
        for (input, expected) in [
            ("0", 0),
            ("5", 5),
            ("+5", 5),
            ("007", 7),
            ("2147483647", 2_147_483_647),
        ] {
            assert_eq!(
                input.parse::<HashOrHeight>(),
                Ok(HashOrHeight::Height(height(expected))),
                "{input:?}"
            );
        }
        for input in ["2147483648", "-1", " 1", "1 ", "0x10", "", "1.0"] {
            assert!(input.parse::<HashOrHeight>().is_err(), "{input:?}");
        }
    }

    #[test]
    fn a_negative_height_counts_back_from_the_tip() {
        let tip = Some(height(100));
        assert_eq!(
            HashOrHeight::parse_relative("-1", tip),
            Ok(HashOrHeight::Height(height(100)))
        );
        assert_eq!(
            HashOrHeight::parse_relative("-100", tip),
            Ok(HashOrHeight::Height(height(1)))
        );
        assert_eq!(
            HashOrHeight::parse_relative("-101", tip),
            Err(HashOrHeightParseError::BeforeGenesis {
                offset: -101,
                tip: height(100)
            })
        );
        assert_eq!(
            HashOrHeight::parse_relative("-1", None),
            Err(HashOrHeightParseError::MissingTip)
        );
        assert!(HashOrHeight::parse_relative("-0", tip).is_err());
        assert_eq!(
            HashOrHeight::parse_relative("7", None),
            Ok(HashOrHeight::Height(height(7)))
        );
    }

    #[test]
    fn display_round_trips_both_forms() {
        for input in [DISPLAY_HASH, "123"] {
            let parsed: HashOrHeight = input.parse().expect("a golden input parses");
            assert_eq!(parsed.to_string(), input);
        }
    }
}
