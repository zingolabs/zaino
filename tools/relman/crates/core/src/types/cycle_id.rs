use std::fmt;

/// A release-cycle identifier — the stable human handle for a release period
/// (e.g. `2026-08-15` or `cycle-42`), independent of any version.
///
/// Invariants, enforced once at [`parse`](CycleId::parse):
/// - non-empty,
/// - every character is a lowercase ASCII letter, digit, or `-`.
///
/// Parse-don't-validate: once you hold a `CycleId`, the string is known good,
/// so it composes into git tag names without further checking.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CycleId(String);

/// Why a string was rejected as a [`CycleId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCycleId {
    /// The input was the empty string.
    #[error("cycle id is empty")]
    Empty,
    /// A character outside the allowed set appeared.
    #[error(
        "cycle id contains an invalid character {found:?} \
         (allowed: lowercase ASCII letters, digits, '-')"
    )]
    InvalidChar {
        /// The offending character.
        found: char,
    },
}

impl CycleId {
    /// Parse a cycle id, enforcing the invariants documented on the type.
    pub fn parse(raw: &str) -> Result<Self, InvalidCycleId> {
        if raw.is_empty() {
            return Err(InvalidCycleId::Empty);
        }
        for ch in raw.chars() {
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
                return Err(InvalidCycleId::InvalidChar { found: ch });
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the validated id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CycleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_date_and_named_cycles() {
        for raw in ["2026-08-15", "cycle-42", "rc", "0"] {
            let id = CycleId::parse(raw).expect("should be valid");
            assert_eq!(id.as_str(), raw);
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(CycleId::parse(""), Err(InvalidCycleId::Empty));
    }

    #[test]
    fn rejects_uppercase_and_symbols_and_spaces() {
        assert_eq!(
            CycleId::parse("Cycle-1"),
            Err(InvalidCycleId::InvalidChar { found: 'C' })
        );
        assert_eq!(
            CycleId::parse("cycle_1"),
            Err(InvalidCycleId::InvalidChar { found: '_' })
        );
        assert_eq!(
            CycleId::parse("cycle 1"),
            Err(InvalidCycleId::InvalidChar { found: ' ' })
        );
        assert_eq!(
            CycleId::parse("2026.08"),
            Err(InvalidCycleId::InvalidChar { found: '.' })
        );
    }
}
