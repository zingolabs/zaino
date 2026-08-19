/// A changeset filename stem: lowercase ASCII words joined by `-`
/// (e.g. `wandering-quokka`).
///
/// Invariants, enforced once at [`parse`](Slug::parse):
/// - non-empty,
/// - every character is a lowercase ASCII letter, digit, or `-`,
/// - no leading, trailing, or doubled `-`.
///
/// Parse-don't-validate: once you hold a `Slug`, the stem is known good, so
/// consumers never re-check it. A slug names exactly one `.changesets/<slug>.toml`
/// file via [`file_name`](Slug::file_name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Slug(String);

/// Why a string was rejected as a [`Slug`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSlug {
    /// The input was the empty string.
    #[error("slug is empty")]
    Empty,
    /// A character outside the allowed set appeared.
    #[error("slug contains an invalid character {found:?} (allowed: 'a'-'z', '0'-'9', '-')")]
    InvalidChar {
        /// The offending character.
        found: char,
    },
    /// The slug started or ended with `-`.
    #[error("slug must not start or end with '-', found {0:?}")]
    BoundaryDash(String),
    /// The slug contained a `--` doubled separator.
    #[error("slug must not contain a doubled '--', found {0:?}")]
    DoubleDash(String),
}

impl Slug {
    /// Parse a slug, enforcing the invariants documented on the type.
    pub fn parse(raw: &str) -> Result<Self, InvalidSlug> {
        if raw.is_empty() {
            return Err(InvalidSlug::Empty);
        }
        for ch in raw.chars() {
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
                return Err(InvalidSlug::InvalidChar { found: ch });
            }
        }
        if raw.starts_with('-') || raw.ends_with('-') {
            return Err(InvalidSlug::BoundaryDash(raw.to_owned()));
        }
        if raw.contains("--") {
            return Err(InvalidSlug::DoubleDash(raw.to_owned()));
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the validated stem.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The changeset file name this slug maps to: `<slug>.toml`.
    pub fn file_name(&self) -> String {
        format!("{}.toml", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_slugs() {
        for raw in [
            "wandering-quokka",
            "brisk-heron",
            "a",
            "a1",
            "one-two-three",
            "x9",
        ] {
            let parsed = Slug::parse(raw).expect("should be valid");
            assert_eq!(parsed.as_str(), raw);
        }
    }

    #[test]
    fn file_name_appends_toml() {
        let slug = Slug::parse("wandering-quokka").expect("valid");
        assert_eq!(slug.file_name(), "wandering-quokka.toml");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Slug::parse(""), Err(InvalidSlug::Empty));
    }

    #[test]
    fn rejects_uppercase_and_other_chars() {
        assert_eq!(
            Slug::parse("Wandering"),
            Err(InvalidSlug::InvalidChar { found: 'W' })
        );
        assert_eq!(
            Slug::parse("wandering_quokka"),
            Err(InvalidSlug::InvalidChar { found: '_' })
        );
        assert_eq!(
            Slug::parse("wandering quokka"),
            Err(InvalidSlug::InvalidChar { found: ' ' })
        );
    }

    #[test]
    fn rejects_boundary_dash() {
        assert_eq!(
            Slug::parse("-quokka"),
            Err(InvalidSlug::BoundaryDash("-quokka".to_owned()))
        );
        assert_eq!(
            Slug::parse("quokka-"),
            Err(InvalidSlug::BoundaryDash("quokka-".to_owned()))
        );
    }

    #[test]
    fn rejects_double_dash() {
        assert_eq!(
            Slug::parse("wandering--quokka"),
            Err(InvalidSlug::DoubleDash("wandering--quokka".to_owned()))
        );
    }
}
