use std::sync::Mutex;

use crate::ports::SlugSource;
use crate::types::Slug;

/// A [`SlugSource`] that returns a preset sequence of slugs in order, cycling
/// back to the start once exhausted.
///
/// Deterministic stand-in for the random adapter: seed it with the exact slugs
/// a test wants `generate()` to hand out (e.g. a colliding first slug followed
/// by a free second one).
pub struct SequenceSlugSource {
    slugs: Vec<Slug>,
    next: Mutex<usize>,
}

impl SequenceSlugSource {
    /// Build a source over a non-empty sequence.
    ///
    /// Panics if `slugs` is empty — a source with nothing to hand out is a test
    /// wiring bug, not a runtime condition, so the invariant is asserted.
    pub fn new(slugs: Vec<Slug>) -> Self {
        assert!(
            !slugs.is_empty(),
            "SequenceSlugSource needs at least one slug"
        );
        Self {
            slugs,
            next: Mutex::new(0),
        }
    }
}

impl SlugSource for SequenceSlugSource {
    fn generate(&self) -> Slug {
        let mut next = self.next.lock().expect("SequenceSlugSource mutex poisoned");
        let slug = self.slugs[*next % self.slugs.len()].clone();
        *next += 1;
        slug
    }
}
