//! Common types used across jsonrpsee responses

pub mod amount;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The identifier for a Zcash node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub i64);

/// The height of a Zcash block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockHeight(pub u32);

impl From<u32> for BlockHeight {
    fn from(v: u32) -> Self {
        BlockHeight(v)
    }
}

/// The height of a Zcash block, or None if unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaybeHeight(pub Option<BlockHeight>);

impl Serialize for MaybeHeight {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(BlockHeight(h)) => ser.serialize_u32(h),
            None => ser.serialize_i64(-1),
        }
    }
}

impl<'de> Deserialize<'de> for MaybeHeight {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // Accept either a number or null.
        // Negative → None; non-negative → Some(height).
        let opt = Option::<i64>::deserialize(de)?;
        match opt {
            None => Ok(MaybeHeight(None)),
            Some(n) if n < 0 => Ok(MaybeHeight(None)),
            Some(n) => {
                let h = u32::try_from(n).map_err(serde::de::Error::custom)?;
                Ok(MaybeHeight(Some(BlockHeight(h))))
            }
        }
    }
}

/// Unix timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixTime(pub i64);

impl UnixTime {
    /// Converts to a [`SystemTime`].
    pub fn as_system_time(self) -> SystemTime {
        if self.0 >= 0 {
            UNIX_EPOCH + Duration::from_secs(self.0 as u64)
        } else {
            UNIX_EPOCH - Duration::from_secs(self.0.unsigned_abs())
        }
    }
}

/// Duration in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecondsF64(pub f64);

impl SecondsF64 {
    /// Converts to a [`Duration`].
    pub fn as_duration(self) -> Duration {
        Duration::from_secs_f64(self.0)
    }
}

/// Protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub i64);

/// A byte array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bytes(pub u64);

/// Time offset in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimeOffsetSeconds(pub i64);

#[cfg(test)]
mod tests {
    use crate::jsonrpsee::response::common::{BlockHeight, MaybeHeight};

    #[test]
    fn maybeheight_deser_accepts_minus_one_and_null() {
        let a: MaybeHeight = serde_json::from_str("-1").unwrap();
        assert!(a.0.is_none());

        let b: MaybeHeight = serde_json::from_str("null").unwrap();
        assert!(b.0.is_none());

        let c: MaybeHeight = serde_json::from_str("123").unwrap();
        assert_eq!(c.0.unwrap().0, 123);
    }

    #[test]
    fn maybeheight_serializes_none_as_minus_one() {
        let m = MaybeHeight(None);
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "-1");
    }

    #[test]
    fn maybeheight_roundtrips_some() {
        let m = MaybeHeight(Some(BlockHeight(42)));
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "42");
        let back: MaybeHeight = serde_json::from_str(&s).unwrap();
        assert_eq!(back.0.unwrap().0, 42);
    }
}
