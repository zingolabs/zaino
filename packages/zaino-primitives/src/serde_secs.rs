//! Config `*_secs` keys: whole seconds on the wire, `Duration` in code (`#[serde(with = …)]`)

use std::num::NonZeroU64;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serializer};

/// Any whole seconds, 0 included
pub mod any {
    use super::*;

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_secs)
    }
}

/// 0 refused at parse
pub mod nonzero {
    use super::*;

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        any::serialize(duration, serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        NonZeroU64::deserialize(deserializer).map(|secs| Duration::from_secs(secs.get()))
    }
}

/// 0 = `None` (never)
pub mod zero_is_never {
    use super::*;

    pub fn serialize<S: Serializer>(
        duration: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(duration.map_or(0, |duration| duration.as_secs()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        let secs = u64::deserialize(deserializer)?;
        Ok((secs > 0).then(|| Duration::from_secs(secs)))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Deserialize, Serialize)]
    struct Keys {
        #[serde(with = "any")]
        grace_secs: Duration,
        #[serde(with = "nonzero")]
        timeout_secs: Duration,
        #[serde(with = "zero_is_never")]
        age_secs: Option<Duration>,
    }

    /// Whole seconds both ways (0 = zero, refused, never: per adapter)
    #[test]
    fn each_adapter_reads_and_writes_whole_seconds_with_its_zero_rule() {
        let secs = Duration::from_secs;
        let table = "grace_secs = 0\ntimeout_secs = 30\nage_secs = 0\n";
        let parsed: Keys = toml::from_str(table).expect("parses");
        let expected = Keys { grace_secs: secs(0), timeout_secs: secs(30), age_secs: None };
        assert_eq!(parsed, expected);
        assert_eq!(toml::to_string(&parsed).expect("serialises"), table, "round trip");

        let aged: Keys = toml::from_str("grace_secs = 600\ntimeout_secs = 1\nage_secs = 1800\n")
            .expect("parses");
        let expected =
            Keys { grace_secs: secs(600), timeout_secs: secs(1), age_secs: Some(secs(1800)) };
        assert_eq!(aged, expected, "nonzero seconds read as themselves");

        let zero = toml::from_str::<Keys>("grace_secs = 0\ntimeout_secs = 0\nage_secs = 0\n");
        let refused = zero.expect_err("timeout_secs = 0 refused").to_string();
        assert!(refused.contains("nonzero"), "refusal names the nonzero rule: {refused}");
    }
}
