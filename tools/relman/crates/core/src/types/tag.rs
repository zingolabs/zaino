use std::fmt;

use crate::types::{CrateName, CycleId, Version};

/// A validated git tag name.
///
/// relman constructs the three release-identity tag shapes from already-typed
/// parts ([`crate_version`](Tag::crate_version), [`cycle`](Tag::cycle),
/// [`cycle_rc`](Tag::cycle_rc)) — those are infallible because their inputs are
/// pre-validated. [`parse`](Tag::parse) is the fallible door for arbitrary
/// strings, rejecting names git itself would refuse (`check-ref-format` rules,
/// pared to what a tag name needs).
///
/// Parse-don't-validate: once you hold a `Tag`, the string is a usable tag name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag(String);

/// Why a string was rejected as a [`Tag`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidTag {
    /// The input was the empty string.
    #[error("tag name is empty")]
    Empty,
    /// The name began with a character git forbids at the start of a ref
    /// component (`-`, `.`, or `/`).
    #[error("tag name must not start with {found:?}")]
    LeadingChar {
        /// The offending first character.
        found: char,
    },
    /// A character outside the allowed set appeared. relman tags use only ASCII
    /// letters, digits, and `-`, `.`, `_`, `/`.
    #[error(
        "tag name contains an invalid character {found:?} \
         (allowed: ASCII letters, digits, '-', '.', '_', '/')"
    )]
    InvalidChar {
        /// The offending character.
        found: char,
    },
    /// The name contained `..`, which git rejects in a ref.
    #[error("tag name must not contain '..'")]
    DoubleDot,
}

impl Tag {
    /// Wrap a string produced from already-validated parts. Private: the only
    /// external door is [`parse`](Tag::parse); the constructors below feed this
    /// names they know are well-formed.
    fn from_valid(name: String) -> Self {
        Self(name)
    }

    /// The per-crate provenance tag `"{crate}-v{version}"` (e.g.
    /// `zaino-state-v0.4.0`) — one git point per published `crate@version`.
    pub fn crate_version(crate_name: &CrateName, version: &Version) -> Self {
        Self::from_valid(format!("{crate_name}-v{version}"))
    }

    /// The cycle (period) tag `"cycle-{id}"` (e.g. `cycle-2026-08-15`) applied
    /// at blessing — the stable human handle carrying no version.
    pub fn cycle(cycle: &CycleId) -> Self {
        Self::from_valid(format!("cycle-{cycle}"))
    }

    /// The soak-prerelease tag `"cycle-{id}-rc.{n}"` (e.g.
    /// `cycle-2026-08-15-rc.6`) applied to each release-candidate cut.
    pub fn cycle_rc(cycle: &CycleId, n: u32) -> Self {
        Self::from_valid(format!("cycle-{cycle}-rc.{n}"))
    }

    /// Parse an arbitrary string as a git tag name, enforcing the invariants
    /// documented on [`InvalidTag`].
    pub fn parse(raw: &str) -> Result<Self, InvalidTag> {
        let first = raw.chars().next().ok_or(InvalidTag::Empty)?;
        if matches!(first, '-' | '.' | '/') {
            return Err(InvalidTag::LeadingChar { found: first });
        }
        if raw.contains("..") {
            return Err(InvalidTag::DoubleDot);
        }
        for ch in raw.chars() {
            if !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '/')) {
                return Err(InvalidTag::InvalidChar { found: ch });
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the validated tag name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn cycle_id(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid cycle id")
    }

    #[test]
    fn crate_version_tag_is_crate_dash_v_version() {
        let tag = Tag::crate_version(&crate_name("zaino-state"), &version("0.4.0"));
        assert_eq!(tag.as_str(), "zaino-state-v0.4.0");
    }

    #[test]
    fn cycle_tag_is_cycle_dash_id() {
        let tag = Tag::cycle(&cycle_id("2026-08-15"));
        assert_eq!(tag.as_str(), "cycle-2026-08-15");
    }

    #[test]
    fn cycle_rc_tag_is_cycle_dash_id_dash_rc_dot_n() {
        let tag = Tag::cycle_rc(&cycle_id("2026-08-15"), 6);
        assert_eq!(tag.as_str(), "cycle-2026-08-15-rc.6");
    }

    #[test]
    fn parse_accepts_the_constructed_shapes() {
        for raw in [
            "zaino-state-v0.4.0",
            "cycle-2026-08-15",
            "cycle-2026-08-15-rc.6",
            "v1.2.3",
        ] {
            assert_eq!(Tag::parse(raw).expect("valid").as_str(), raw);
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert_eq!(Tag::parse(""), Err(InvalidTag::Empty));
        assert_eq!(
            Tag::parse("-leading"),
            Err(InvalidTag::LeadingChar { found: '-' })
        );
        assert_eq!(
            Tag::parse(".leading"),
            Err(InvalidTag::LeadingChar { found: '.' })
        );
        assert_eq!(Tag::parse("a..b"), Err(InvalidTag::DoubleDot));
        assert_eq!(
            Tag::parse("has space"),
            Err(InvalidTag::InvalidChar { found: ' ' })
        );
        assert_eq!(
            Tag::parse("caret^tag"),
            Err(InvalidTag::InvalidChar { found: '^' })
        );
    }
}
