//! Types associated with the `getblockhash` RPC request.

use core::fmt;

use serde::{de::Visitor, Deserialize, Deserializer, Serialize, Serializer};
use zebra_chain::block::Height;

/// Block index argument to `getblockhash`.
///
/// Mirrors zcashd's convention where `-1` selects the chain tip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockSelector {
    /// The current chain tip.
    Tip,
    /// An absolute block height.
    Height(Height),
}

impl BlockSelector {
    /// Resolve to a concrete height given the current tip.
    #[inline]
    pub fn resolve(self, tip: Height) -> Height {
        match self {
            BlockSelector::Tip => tip,
            BlockSelector::Height(h) => h,
        }
    }

    /// Convenience: returns `Some(h)` if absolute, else `None`.
    #[inline]
    pub fn height(self) -> Option<Height> {
        match self {
            BlockSelector::Tip => None,
            BlockSelector::Height(h) => Some(h),
        }
    }
}

impl<'de> Deserialize<'de> for BlockSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SelVisitor;

        impl<'de> Visitor<'de> for SelVisitor {
            type Value = BlockSelector;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    f,
                    "an integer height ≥ 0, -1 for tip, or a string like \"tip\"/\"-1\"/\"42\""
                )
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v == -1 {
                    Ok(BlockSelector::Tip)
                } else if v >= 0 && v <= u32::MAX as i64 {
                    Ok(BlockSelector::Height(Height(v as u32)))
                } else {
                    Err(E::custom("block height out of range"))
                }
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v <= u32::MAX as u64 {
                    Ok(BlockSelector::Height(Height(v as u32)))
                } else {
                    Err(E::custom("block height out of range"))
                }
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let s = s.trim();
                if s.eq_ignore_ascii_case("tip") {
                    return Ok(BlockSelector::Tip);
                }
                let v: i64 = s
                    .parse()
                    .map_err(|_| E::custom("invalid block index string"))?;
                self.visit_i64(v)
            }
        }

        deserializer.deserialize_any(SelVisitor)
    }
}

impl Serialize for BlockSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            BlockSelector::Tip => serializer.serialize_i64(-1), // mirrors zcashd “-1 = tip”
            BlockSelector::Height(h) => serializer.serialize_u64(h.0 as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{self, json};

    #[test]
    fn deserialize_numbers_succeeds() {
        // JSON numbers
        let selector_from_negative_one: BlockSelector = serde_json::from_str("-1").unwrap();
        assert_eq!(selector_from_negative_one, BlockSelector::Tip);

        let selector_from_spaced_negative_one: BlockSelector =
            serde_json::from_str("  -1 ").unwrap();
        assert_eq!(selector_from_spaced_negative_one, BlockSelector::Tip);

        let selector_from_zero: BlockSelector = serde_json::from_str("0").unwrap();
        assert_eq!(selector_from_zero, BlockSelector::Height(Height(0)));

        let selector_from_forty_two: BlockSelector = serde_json::from_str("42").unwrap();
        assert_eq!(selector_from_forty_two, BlockSelector::Height(Height(42)));

        let selector_from_max_u32: BlockSelector =
            serde_json::from_str(&u32::MAX.to_string()).unwrap();
        assert_eq!(
            selector_from_max_u32,
            BlockSelector::Height(Height(u32::MAX))
        );
    }

    #[test]
    fn deserialize_strings_succeeds() {
        // JSON strings
        let selector_from_tip_literal: BlockSelector = serde_json::from_str(r#""tip""#).unwrap();
        assert_eq!(selector_from_tip_literal, BlockSelector::Tip);

        let selector_from_case_insensitive_tip: BlockSelector =
            serde_json::from_str(r#"" TIP ""#).unwrap();
        assert_eq!(selector_from_case_insensitive_tip, BlockSelector::Tip);

        let selector_from_negative_one_string: BlockSelector =
            serde_json::from_str(r#""-1""#).unwrap();
        assert_eq!(selector_from_negative_one_string, BlockSelector::Tip);

        let selector_from_numeric_string: BlockSelector = serde_json::from_str(r#""42""#).unwrap();
        assert_eq!(
            selector_from_numeric_string,
            BlockSelector::Height(Height(42))
        );

        let selector_from_spaced_numeric_string: BlockSelector =
            serde_json::from_str(r#""  17  ""#).unwrap();
        assert_eq!(
            selector_from_spaced_numeric_string,
            BlockSelector::Height(Height(17))
        );
    }

    #[test]
    fn deserialize_with_invalid_inputs_fails() {
        // Numbers: invalid negative and too large
        assert!(serde_json::from_str::<BlockSelector>("-2").is_err());
        assert!(serde_json::from_str::<BlockSelector>("9223372036854775807").is_err());

        // Strings: invalid negative, too large, and malformed
        assert!(serde_json::from_str::<BlockSelector>(r#""-2""#).is_err());

        let value_exceeding_u32_maximum = (u32::MAX as u64 + 1).to_string();
        let json_string_exceeding_u32_maximum = format!(r#""{}""#, value_exceeding_u32_maximum);
        assert!(serde_json::from_str::<BlockSelector>(&json_string_exceeding_u32_maximum).is_err());

        assert!(serde_json::from_str::<BlockSelector>(r#""nope""#).is_err());
        assert!(serde_json::from_str::<BlockSelector>(r#""""#).is_err());
    }

    #[test]
    fn serialize_values_match_expected_representations() {
        let json_value_for_tip = serde_json::to_value(BlockSelector::Tip).unwrap();
        assert_eq!(json_value_for_tip, json!(-1));

        let json_value_for_zero_height =
            serde_json::to_value(BlockSelector::Height(Height(0))).unwrap();
        assert_eq!(json_value_for_zero_height, json!(0));

        let json_value_for_specific_height =
            serde_json::to_value(BlockSelector::Height(Height(42))).unwrap();
        assert_eq!(json_value_for_specific_height, json!(42));

        let json_value_for_maximum_height =
            serde_json::to_value(BlockSelector::Height(Height(u32::MAX))).unwrap();
        assert_eq!(json_value_for_maximum_height, json!(u32::MAX as u64));
    }

    #[test]
    fn json_round_trip_preserves_value() {
        let test_cases = [
            BlockSelector::Tip,
            BlockSelector::Height(Height(0)),
            BlockSelector::Height(Height(1)),
            BlockSelector::Height(Height(42)),
            BlockSelector::Height(Height(u32::MAX)),
        ];

        for test_case in test_cases {
            let serialized_json_string = serde_json::to_string(&test_case).unwrap();
            let round_tripped_selector: BlockSelector =
                serde_json::from_str(&serialized_json_string).unwrap();
            assert_eq!(
                round_tripped_selector, test_case,
                "Round trip failed for {test_case:?} via {serialized_json_string}"
            );
        }
    }

    #[test]
    fn resolve_and_helper_methods_work_as_expected() {
        let tip_height = Height(100);

        // Tip resolves to the current tip height
        let selector_tip = BlockSelector::Tip;
        assert_eq!(selector_tip.resolve(tip_height), tip_height);
        assert_eq!(selector_tip.height(), None);

        // Absolute height resolves to itself
        let selector_absolute_height = BlockSelector::Height(Height(90));
        assert_eq!(selector_absolute_height.resolve(tip_height), Height(90));
        assert_eq!(selector_absolute_height.height(), Some(Height(90)));
    }
}
