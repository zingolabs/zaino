//! `z_getsubtreesbyindex` — a run of complete note-commitment subtree roots.

use crate::types::{ShieldedPool, SubtreeRoot};

/// A contiguous run of complete subtree roots from one shielded pool.
///
/// The pool and starting index are carried alongside the roots because the
/// response echoes back what it answered: a caller paging through subtrees needs
/// to know where the run it received actually began, which may differ from where
/// it asked if the index was clamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeRoots {
    /// The pool these subtrees belong to.
    pub pool: ShieldedPool,

    /// Index of the first subtree in [`Self::subtrees`].
    pub start_index: u16,

    /// The roots, in ascending index order and contiguous from
    /// [`Self::start_index`].
    pub subtrees: Vec<SubtreeRoot>,
}
