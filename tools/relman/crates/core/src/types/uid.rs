use std::fmt;

/// A changeset's immutable unique identity: a UUID in canonical hyphenated
/// lowercase form (e.g. `018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f`).
///
/// Assigned once at changeset creation and never rewritten, so it is a stable
/// handle for a changeset independent of its slug (which the PR-rename bot
/// changes) or its `consumed_in` mark (which a release stamps). Nothing consumes
/// the id yet — it is baked in now, pre-launch, because retrofitting stable
/// identity onto already-shipped changesets later is painful.
///
/// Invariants, enforced once at [`parse`](Uid::parse):
/// - exactly 36 characters in the canonical `8-4-4-4-12` layout,
/// - hyphens at positions 8, 13, 18, and 23,
/// - every other character a lowercase ASCII hex digit (`0`-`9`, `a`-`f`).
///
/// Parse-don't-validate: once you hold a `Uid`, the string is known good. The
/// newtype deliberately does *not* generate UUIDs — generation lives in the
/// adapter ([`UidSource`](crate::ports::UidSource)), keeping the random/time
/// dependency out of the core.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uid(String);

/// Why a string was rejected as a [`Uid`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidUid {
    /// The input was the empty string.
    #[error("uid is empty")]
    Empty,
    /// The input was not exactly 36 characters, so it cannot be the canonical
    /// hyphenated form.
    #[error("uid must be 36 characters in canonical UUID form, found {found} characters")]
    WrongLength {
        /// The rejected length.
        found: usize,
    },
    /// A character was not the one the canonical layout requires at its
    /// position — a hyphen expected where a hex digit appeared (or vice versa),
    /// or a non-lowercase-hex character.
    #[error(
        "uid has an invalid character {found:?} at position {position} \
         (canonical form is '8-4-4-4-12' lowercase hex)"
    )]
    InvalidChar {
        /// The zero-based character position of the offending character.
        position: usize,
        /// The offending character.
        found: char,
    },
}

impl Uid {
    /// Parse a uid, enforcing the canonical-UUID invariants documented on the
    /// type. Accepts only the hyphenated lowercase form; anything else (upper
    /// case, braces, urn prefix, wrong length) is rejected.
    pub fn parse(raw: &str) -> Result<Self, InvalidUid> {
        if raw.is_empty() {
            return Err(InvalidUid::Empty);
        }
        if raw.chars().count() != 36 {
            return Err(InvalidUid::WrongLength {
                found: raw.chars().count(),
            });
        }
        for (position, ch) in raw.chars().enumerate() {
            let expects_hyphen = matches!(position, 8 | 13 | 18 | 23);
            let valid = if expects_hyphen {
                ch == '-'
            } else {
                ch.is_ascii_digit() || ('a'..='f').contains(&ch)
            };
            if !valid {
                return Err(InvalidUid::InvalidChar {
                    position,
                    found: ch,
                });
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the validated uid.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_uuids() {
        for raw in [
            "018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f",
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ] {
            let uid = Uid::parse(raw).expect("should be valid");
            assert_eq!(uid.as_str(), raw);
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Uid::parse(""), Err(InvalidUid::Empty));
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            Uid::parse("018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e"),
            Err(InvalidUid::WrongLength { found: 34 })
        );
    }

    #[test]
    fn rejects_uppercase() {
        // Position 0 is the first upper-case hex character.
        assert_eq!(
            Uid::parse("018F4E0A-7b2c-7c3d-8e4f-1a2b3c4d5e6f"),
            Err(InvalidUid::InvalidChar {
                position: 3,
                found: 'F'
            })
        );
    }

    #[test]
    fn rejects_misplaced_hyphen() {
        // A hex digit where the layout demands a hyphen (position 8).
        assert_eq!(
            Uid::parse("018f4e0a07b2c-7c3d-8e4f-1a2b3c4d5e6f"),
            Err(InvalidUid::InvalidChar {
                position: 8,
                found: '0'
            })
        );
    }

    #[test]
    fn rejects_non_hex_character() {
        assert_eq!(
            Uid::parse("018f4e0g-7b2c-7c3d-8e4f-1a2b3c4d5e6f"),
            Err(InvalidUid::InvalidChar {
                position: 7,
                found: 'g'
            })
        );
    }
}
