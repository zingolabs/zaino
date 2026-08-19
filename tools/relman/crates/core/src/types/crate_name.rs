use std::fmt;

/// A validated Cargo crate name.
///
/// Invariants, enforced once at [`parse`](CrateName::parse):
/// - non-empty,
/// - every character is an ASCII letter, digit, `-`, or `_`,
/// - the first character is an ASCII letter.
///
/// Parse-don't-validate: once you hold a `CrateName`, the string is known
/// good, so consumers never re-check it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateName(String);

/// Why a string was rejected as a [`CrateName`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCrateName {
    /// The input was the empty string.
    #[error("crate name is empty")]
    Empty,
    /// The first character was not an ASCII letter.
    #[error("crate name must start with an ASCII letter, found {found:?}")]
    LeadingChar {
        /// The offending first character.
        found: char,
    },
    /// A character outside the allowed set appeared.
    #[error(
        "crate name contains an invalid character {found:?} \
         (allowed: ASCII letters, digits, '-', '_')"
    )]
    InvalidChar {
        /// The offending character.
        found: char,
    },
}

impl CrateName {
    /// Parse a crate name, enforcing the invariants documented on the type.
    pub fn parse(raw: &str) -> Result<Self, InvalidCrateName> {
        let first = raw.chars().next().ok_or(InvalidCrateName::Empty)?;
        if !first.is_ascii_alphabetic() {
            return Err(InvalidCrateName::LeadingChar { found: first });
        }
        for ch in raw.chars() {
            if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
                return Err(InvalidCrateName::InvalidChar { found: ch });
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the validated name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CrateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_names() {
        for name in ["zaino-state", "zainod", "zaino_common", "z", "a1", "x-2_y"] {
            let parsed = CrateName::parse(name).expect("should be valid");
            assert_eq!(parsed.as_str(), name);
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(CrateName::parse(""), Err(InvalidCrateName::Empty));
    }

    #[test]
    fn rejects_leading_non_letter() {
        assert_eq!(
            CrateName::parse("1abc"),
            Err(InvalidCrateName::LeadingChar { found: '1' })
        );
        assert_eq!(
            CrateName::parse("-abc"),
            Err(InvalidCrateName::LeadingChar { found: '-' })
        );
        assert_eq!(
            CrateName::parse("_abc"),
            Err(InvalidCrateName::LeadingChar { found: '_' })
        );
    }

    #[test]
    fn rejects_invalid_char() {
        assert_eq!(
            CrateName::parse("zaino.state"),
            Err(InvalidCrateName::InvalidChar { found: '.' })
        );
        assert_eq!(
            CrateName::parse("zaino state"),
            Err(InvalidCrateName::InvalidChar { found: ' ' })
        );
        assert_eq!(
            CrateName::parse("zaino/state"),
            Err(InvalidCrateName::InvalidChar { found: '/' })
        );
    }
}
