use std::sync::Mutex;

use crate::ports::UidSource;
use crate::types::Uid;

/// A [`UidSource`] that returns a preset sequence of uids in order, cycling back
/// to the start once exhausted.
///
/// Deterministic stand-in for the random adapter: seed it with the exact uids a
/// test wants `generate()` to hand out, so a created changeset carries a known
/// identity.
pub struct SequenceUidSource {
    uids: Vec<Uid>,
    next: Mutex<usize>,
}

impl SequenceUidSource {
    /// Build a source over a non-empty sequence.
    ///
    /// Panics if `uids` is empty — a source with nothing to hand out is a test
    /// wiring bug, not a runtime condition, so the invariant is asserted.
    pub fn new(uids: Vec<Uid>) -> Self {
        assert!(!uids.is_empty(), "SequenceUidSource needs at least one uid");
        Self {
            uids,
            next: Mutex::new(0),
        }
    }
}

impl UidSource for SequenceUidSource {
    fn generate(&self) -> Uid {
        let mut next = self.next.lock().expect("SequenceUidSource mutex poisoned");
        let uid = self.uids[*next % self.uids.len()].clone();
        *next += 1;
        uid
    }
}
