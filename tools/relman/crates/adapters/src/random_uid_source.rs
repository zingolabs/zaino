use uuid::Uuid;

use relman_core::ports::UidSource;
use relman_core::types::Uid;

/// A [`UidSource`] that mints a fresh time-sortable UUIDv7 per call.
///
/// UUIDv7 embeds a millisecond timestamp in its high bits, so ids sort roughly
/// in creation order — handy for a changeset ledger — while retaining random
/// low bits for uniqueness. Formatted hyphenated-lowercase, which is exactly the
/// canonical form [`Uid::parse`] accepts.
pub struct RandomUidSource;

impl RandomUidSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RandomUidSource {
    fn default() -> Self {
        Self::new()
    }
}

impl UidSource for RandomUidSource {
    fn generate(&self) -> Uid {
        // `Uuid::now_v7` renders to the canonical 36-char hyphenated lowercase
        // form, which `Uid::parse` accepts by construction — a parse failure
        // here would mean the uuid crate broke that format contract, a genuine
        // invariant break rather than a runtime input error.
        let raw = Uuid::now_v7().hyphenated().to_string();
        Uid::parse(&raw).expect("uuid v7 formats to a valid Uid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_uids() {
        let source = RandomUidSource::new();
        for _ in 0..50 {
            let uid = source.generate();
            // Re-parsing a generated uid must succeed (it already is one).
            assert!(Uid::parse(uid.as_str()).is_ok());
        }
    }

    #[test]
    fn successive_uids_are_distinct() {
        let source = RandomUidSource::new();
        let first = source.generate();
        let second = source.generate();
        assert_ne!(first, second);
    }
}
