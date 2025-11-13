//! Common types for handling ZEC and Zatoshi amounts.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Zatoshis per ZEC.
pub const ZATS_PER_ZEC: u64 = 100_000_000;
/// Represents an amount in Zatoshis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Zatoshis(pub u64);

impl<'de> Deserialize<'de> for Zatoshis {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// For floats, use [`ZecAmount`].
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum IntLike {
            U64(u64),
            I64(i64),
            Str(String),
        }

        match IntLike::deserialize(de)? {
            IntLike::U64(u) => Ok(Zatoshis(u)),
            IntLike::I64(i) if i >= 0 => Ok(Zatoshis(i as u64)),
            IntLike::I64(_) => Err(serde::de::Error::custom("negative amount")),
            IntLike::Str(s) => {
                let s = s.trim();
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(serde::de::Error::custom("expected integer zatoshis"));
                }
                s.parse::<u64>()
                    .map(Zatoshis)
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}

/// Represents a ZEC amount. The amount is stored in zatoshis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZecAmount(u64);

impl ZecAmount {
    /// Returns the amount in zatoshis.
    pub fn as_zatoshis(self) -> u64 {
        self.0
    }

    /// Construct from integer zatoshis.
    pub const fn from_zats(z: u64) -> Self {
        Self(z)
    }

    /// Construct from a ZEC decimal (f64).
    pub fn try_from_zec_f64(zec: f64) -> Result<Self, &'static str> {
        if !zec.is_finite() || zec < 0.0 {
            return Err("invalid amount");
        }
        let z = (zec * ZATS_PER_ZEC as f64).round();
        if z < 0.0 || z > u64::MAX as f64 {
            return Err("overflow");
        }
        Ok(Self(z as u64))
    }
}

impl<'de> Deserialize<'de> for ZecAmount {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum NumLike {
            U64(u64),
            I64(i64),
            F64(f64),
            Str(String),
        }

        match NumLike::deserialize(de)? {
            NumLike::U64(u) => u
                .checked_mul(ZATS_PER_ZEC)
                .map(ZecAmount)
                .ok_or_else(|| serde::de::Error::custom("overflow")),
            NumLike::I64(i) if i >= 0 => (i as u64)
                .checked_mul(ZATS_PER_ZEC)
                .map(ZecAmount)
                .ok_or_else(|| serde::de::Error::custom("overflow")),
            NumLike::I64(_) => Err(serde::de::Error::custom("negative amount")),
            NumLike::F64(f) => {
                if !f.is_finite() || f < 0.0 {
                    return Err(serde::de::Error::custom("invalid amount"));
                }
                Ok(ZecAmount((f * (ZATS_PER_ZEC as f64)).round() as u64))
            }
            NumLike::Str(s) => {
                // Parse "int.frac" with up to 8 fractional digits into zats
                let s = s.trim();
                if s.starts_with('-') {
                    return Err(serde::de::Error::custom("negative amount"));
                }
                let (int, frac) = s.split_once('.').unwrap_or((s, ""));
                if frac.len() > 8 {
                    return Err(serde::de::Error::custom("too many fractional digits"));
                }
                let int_part: u64 = if int.is_empty() {
                    0
                } else {
                    int.parse().map_err(serde::de::Error::custom)?
                };
                let mut frac_buf = frac.as_bytes().to_vec();
                while frac_buf.len() < 8 {
                    frac_buf.push(b'0');
                }
                let frac_part: u64 = if frac_buf.is_empty() {
                    0
                } else {
                    std::str::from_utf8(&frac_buf)
                        .unwrap()
                        .parse()
                        .map_err(serde::de::Error::custom)?
                };
                let base = int_part
                    .checked_mul(ZATS_PER_ZEC)
                    .ok_or_else(|| serde::de::Error::custom("overflow"))?;
                base.checked_add(frac_part)
                    .map(ZecAmount)
                    .ok_or_else(|| serde::de::Error::custom("overflow"))
            }
        }
    }
}

impl Serialize for ZecAmount {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        // Emit a JSON number in ZEC.
        let zec = (self.0 as f64) / 100_000_000.0;
        ser.serialize_f64(zec)
    }
}

impl From<ZecAmount> for u64 {
    fn from(z: ZecAmount) -> u64 {
        z.0
    }
}

#[cfg(test)]
mod tests {

    mod zatoshis {
        use crate::jsonrpsee::response::common::amount::Zatoshis;

        #[test]
        fn zatoshis_integer_number_is_zats() {
            let z: Zatoshis = serde_json::from_str("625000000").unwrap();
            assert_eq!(z.0, 625_000_000);
        }

        #[test]
        fn zatoshis_string_digits_are_zats() {
            let z: Zatoshis = serde_json::from_str(r#""625000000""#).unwrap();
            assert_eq!(z.0, 625_000_000);
        }

        #[test]
        fn zatoshis_rejects_float_number() {
            let result = serde_json::from_str::<Zatoshis>("2.5");
            assert!(result.is_err());
        }

        #[test]
        fn zatoshis_rejects_decimal_string() {
            let err = serde_json::from_str::<Zatoshis>(r#""2.5""#).unwrap_err();
            assert!(err.to_string().contains("expected integer"));
        }

        #[test]
        fn zatoshis_rejects_negative() {
            let err = serde_json::from_str::<Zatoshis>("-1").unwrap_err();
            assert!(err.to_string().contains("negative"));
        }

        #[test]
        fn zatoshis_rejects_non_digit_string() {
            let err = serde_json::from_str::<Zatoshis>(r#""abc""#).unwrap_err();
            assert!(err.to_string().contains("expected integer"));
        }
    }

    mod zecamount {
        use crate::jsonrpsee::response::common::amount::ZecAmount;

        #[test]
        fn zecamount_from_float_decimal() {
            let a: ZecAmount = serde_json::from_str("2.5").unwrap();
            assert_eq!(a.as_zatoshis(), 250_000_000);
        }

        #[test]
        fn zecamount_from_string_decimal() {
            let a: ZecAmount = serde_json::from_str(r#""0.00000001""#).unwrap();
            assert_eq!(a.as_zatoshis(), 1);
        }

        #[test]
        fn zecamount_from_integer_number_interpreted_as_zec() {
            // 2 ZEC
            let a: ZecAmount = serde_json::from_str("2").unwrap();
            assert_eq!(a.as_zatoshis(), 200_000_000);
        }

        #[test]
        fn zecamount_from_integer_string_interpreted_as_zec() {
            // 2 ZEC
            let a: ZecAmount = serde_json::from_str(r#""2""#).unwrap();
            assert_eq!(a.as_zatoshis(), 200_000_000);
        }

        #[test]
        fn zecamount_rejects_negative() {
            let err = serde_json::from_str::<ZecAmount>("-0.1").unwrap_err();
            assert!(
                err.to_string().contains("invalid amount") || err.to_string().contains("negative")
            );
        }

        #[test]
        fn zecamount_rejects_more_than_8_fractional_digits() {
            let err = serde_json::from_str::<ZecAmount>(r#""1.000000000""#).unwrap_err();
            assert!(err.to_string().contains("fractional"));
        }

        #[test]
        fn zecamount_overflow_on_huge_integer_zec() {
            // From u64::MAX ZEC, multiplying by 1e8 should overflow
            let huge = format!("{}", u64::MAX);
            let err = serde_json::from_str::<ZecAmount>(&huge).unwrap_err();
            assert!(
                err.to_string().contains("overflow"),
                "expected overflow, got: {err}"
            );
        }

        #[test]
        fn zecamount_boundary_integer_ok() {
            // Max integer ZEC that fits when scaled: floor(u64::MAX / 1e8)
            let max_int_zec = 184_467_440_737u64;
            let a: ZecAmount = serde_json::from_str(&max_int_zec.to_string()).unwrap();
            assert_eq!(a.as_zatoshis(), 18_446_744_073_700_000_000);
        }

        #[test]
        fn zecamount_overflow_on_large_integer_zec() {
            // Just over the boundary must overflow
            let too_big = 184_467_440_738u64;
            let err = serde_json::from_str::<ZecAmount>(&too_big.to_string()).unwrap_err();
            assert!(err.to_string().contains("overflow"));
        }

        #[test]
        fn zecamount_serializes_as_decimal_number() {
            let a = ZecAmount::from_zats(250_000_000); // 2.5 ZEC
            let s = serde_json::to_string(&a).unwrap();
            // Parse back and compare as f64 to avoid formatting quirks (e.g., 1e-8)
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            let f = v.as_f64().unwrap();
            assert!((f - 2.5).abs() < 1e-12, "serialized {s} parsed {f}");
        }

        #[test]
        fn zecamount_roundtrip_small_fraction() {
            // 1 zat
            let a: ZecAmount = serde_json::from_str(r#""0.00000001""#).unwrap();
            let s = serde_json::to_string(&a).unwrap();
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            let f = v.as_f64().unwrap();
            assert!(
                (f - 0.00000001f64).abs() < 1e-20,
                "serialized {s} parsed {f}"
            );
            assert_eq!(a.as_zatoshis(), 1);
        }
    }
}
