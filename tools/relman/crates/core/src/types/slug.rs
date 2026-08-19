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

    /// The canonical changeset slug for PR number `pr`, `index`-th file.
    ///
    /// The two-phase naming from the changeset-format record: the PR-rename bot
    /// renames a PR's author changeset(s) to `pr-<pr>`. A PR carrying more than
    /// one changeset needs conflict-free names, so the first file (`index == 0`)
    /// takes the bare `pr-<pr>`, the second `pr-<pr>-2`, the third `pr-<pr>-3`,
    /// and so on. The result is always a valid [`Slug`], so it is constructed
    /// directly rather than through [`parse`](Slug::parse).
    pub fn for_pr(pr: u32, index: usize) -> Self {
        if index == 0 {
            Self(format!("pr-{pr}"))
        } else {
            Self(format!("pr-{pr}-{}", index + 1))
        }
    }

    /// Whether this slug is a canonical PR name — `pr-<digits>`, optionally with
    /// a `-<digits>` ordinal suffix (`pr-1501`, `pr-1501-2`).
    ///
    /// These are the names the PR-rename bot assigns; a PR's author slugs (the
    /// random `adjective-noun`) never match, so this is how `rename_to_pr`
    /// distinguishes files it still owns from ones already made canonical (an
    /// accumulated changeset from an earlier merged PR). Matches the predicate
    /// `^pr-\d+(-\d+)?$`.
    pub fn is_canonical_pr(&self) -> bool {
        let Some(rest) = self.0.strip_prefix("pr-") else {
            return false;
        };
        let mut parts = rest.split('-');
        let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let Some(number) = parts.next() else {
            return false;
        };
        if !is_digits(number) {
            return false;
        }
        match parts.next() {
            None => true,
            Some(ordinal) => is_digits(ordinal) && parts.next().is_none(),
        }
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
    fn for_pr_names_the_canonical_slugs() {
        assert_eq!(Slug::for_pr(1501, 0).as_str(), "pr-1501");
        assert_eq!(Slug::for_pr(1501, 1).as_str(), "pr-1501-2");
        assert_eq!(Slug::for_pr(1501, 2).as_str(), "pr-1501-3");
        // Whatever `for_pr` produces is a valid slug.
        for index in 0..4 {
            let s = Slug::for_pr(42, index);
            assert_eq!(Slug::parse(s.as_str()).expect("for_pr yields valid slug"), s);
        }
    }

    #[test]
    fn is_canonical_pr_matches_only_pr_names() {
        for canonical in ["pr-1", "pr-1501", "pr-1501-2", "pr-1501-3", "pr-0"] {
            assert!(
                slug(canonical).is_canonical_pr(),
                "{canonical} should be canonical"
            );
        }
        for author in [
            "wandering-quokka",
            "brisk-heron",
            "pr",
            "pr-quokka",
            "pr-1501-quokka",
            "pr-1501-2-3",
            "prefix-1501",
            "x-pr-1501",
        ] {
            assert!(
                !slug(author).is_canonical_pr(),
                "{author} should not be canonical"
            );
        }
    }

    fn slug(raw: &str) -> Slug {
        Slug::parse(raw).expect("valid test slug")
    }

    #[test]
    fn rejects_double_dash() {
        assert_eq!(
            Slug::parse("wandering--quokka"),
            Err(InvalidSlug::DoubleDash("wandering--quokka".to_owned()))
        );
    }
}
