use rand::seq::IndexedRandom;

use relman_core::ports::SlugSource;
use relman_core::types::Slug;

/// Adjectives for the `adjective-noun` slug form. All lowercase ASCII, so any
/// pairing is a valid [`Slug`].
const ADJECTIVES: &[&str] = &[
    "wandering",
    "brisk",
    "quiet",
    "amber",
    "clever",
    "gentle",
    "mellow",
    "nimble",
    "solar",
    "velvet",
    "wistful",
    "zealous",
];

/// Nouns for the `adjective-noun` slug form.
const NOUNS: &[&str] = &[
    "quokka", "heron", "otter", "falcon", "cedar", "harbor", "lantern", "meadow", "pebble",
    "thistle", "willow", "cirrus",
];

/// A [`SlugSource`] that draws a random `adjective-noun` slug from two small
/// embedded word lists.
///
/// Candidates are *not* guaranteed unique — the domain checks each against the
/// store and retries on collision — but the pairing space is large enough that
/// collisions are rare in practice.
pub struct RandomSlugSource;

impl RandomSlugSource {
    pub fn new() -> Self {
        Self
    }

    /// Draw one word from a non-empty list.
    ///
    /// The lists are compile-time constants and never empty, so `choose`
    /// returning `None` is impossible; the invariant is named rather than
    /// propagated.
    fn pick(words: &[&'static str]) -> &'static str {
        let mut rng = rand::rng();
        words
            .choose(&mut rng)
            .expect("word list is a non-empty constant")
    }
}

impl Default for RandomSlugSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SlugSource for RandomSlugSource {
    fn generate(&self) -> Slug {
        let raw = format!("{}-{}", Self::pick(ADJECTIVES), Self::pick(NOUNS));
        // Every list entry is lowercase ASCII with no dashes, so `adjective-noun`
        // always satisfies the slug invariants; parse failure is impossible.
        Slug::parse(&raw).expect("adjective-noun is always a valid slug")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_adjective_noun_slugs() {
        let source = RandomSlugSource::new();
        for _ in 0..50 {
            let slug = source.generate();
            // Re-parsing a generated slug must succeed (it already is one).
            assert!(Slug::parse(slug.as_str()).is_ok());
            assert_eq!(slug.as_str().matches('-').count(), 1);
        }
    }
}
