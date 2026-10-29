//! Every index keyed by [`IndexKind`]: [`IndexSet`] (which) + [`PerIndex`] (one value each), both
//! iterated in [`IndexKind::ALL`] order

use std::fmt;

use zaino_sync::IndexKind;

const COUNT: usize = IndexKind::ALL.len();
const _: () = assert!(COUNT <= u8::BITS as usize, "IndexSet: one bit per IndexKind");

/// Position in [`IndexKind::ALL`]
fn slot(kind: IndexKind) -> usize {
    let slot = IndexKind::ALL.iter().position(|each| *each == kind);
    slot.expect("IndexKind::ALL lists every kind")
}

/// Bit `slot(kind)` per kind held
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct IndexSet(u8);

impl IndexSet {
    pub(crate) fn contains(self, kind: IndexKind) -> bool {
        self.0 & (1 << slot(kind)) != 0
    }

    pub(crate) fn with(self, kind: IndexKind) -> Self {
        Self(self.0 | (1 << slot(kind)))
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = IndexKind> {
        IndexKind::ALL.into_iter().filter(move |kind| self.contains(*kind))
    }
}

impl FromIterator<IndexKind> for IndexSet {
    fn from_iter<I: IntoIterator<Item = IndexKind>>(kinds: I) -> Self {
        kinds.into_iter().fold(Self::default(), Self::with)
    }
}

/// `IndexSet{"utxo_set", "tree_state"}`
impl fmt::Debug for IndexSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IndexSet")?;
        f.debug_set().entries(self.iter().map(IndexKind::name)).finish()
    }
}

/// `Some` at `slot(kind)` per enabled kind
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PerIndex<T>([Option<T>; COUNT]);

impl<T> Default for PerIndex<T> {
    fn default() -> Self {
        Self(std::array::from_fn(|_| None))
    }
}

impl<T> PerIndex<T> {
    pub(crate) fn get(&self, kind: IndexKind) -> Option<&T> {
        self.0[slot(kind)].as_ref()
    }

    #[cfg(test)]
    pub(crate) fn get_mut(&mut self, kind: IndexKind) -> Option<&mut T> {
        self.0[slot(kind)].as_mut()
    }

    /// Panics: `kind` held already (one value per index)
    pub(crate) fn insert(&mut self, kind: IndexKind, value: T) {
        let held = &mut self.0[slot(kind)];
        assert!(held.is_none(), "{}: twice", kind.name());
        *held = Some(value);
    }

    pub(crate) fn kinds(&self) -> IndexSet {
        self.iter().map(|(kind, _)| kind).collect()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (IndexKind, &T)> {
        let held = IndexKind::ALL.into_iter().zip(&self.0);
        held.filter_map(|(kind, value)| Some((kind, value.as_ref()?)))
    }

    #[cfg(test)]
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (IndexKind, &mut T)> {
        let held = IndexKind::ALL.into_iter().zip(&mut self.0);
        held.filter_map(|(kind, value)| Some((kind, value.as_mut()?)))
    }
}

/// Panics: a kind twice
impl<T> FromIterator<(IndexKind, T)> for PerIndex<T> {
    fn from_iter<I: IntoIterator<Item = (IndexKind, T)>>(values: I) -> Self {
        let mut each = Self::default();
        for (kind, value) in values {
            each.insert(kind, value);
        }
        each
    }
}

/// `{"utxo_set": .., "tree_state": ..}`
impl<T: fmt::Debug> fmt::Debug for PerIndex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter().map(|(kind, value)| (kind.name(), value))).finish()
    }
}
