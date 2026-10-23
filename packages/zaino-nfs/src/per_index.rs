//! One value per enabled index

use zaino_persistence::IndexKind;

/// Order = insert order
#[derive(Debug, Clone)]
pub(crate) struct PerIndex<T>(Vec<(IndexKind, T)>);

impl<T> Default for PerIndex<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> PerIndex<T> {
    pub(crate) fn get(&self, kind: IndexKind) -> Option<&T> {
        self.0.iter().find(|(each, _)| *each == kind).map(|(_, value)| value)
    }

    /// Panics: `kind` held already (one value per index)
    pub(crate) fn insert(&mut self, kind: IndexKind, value: T) {
        assert!(self.get(kind).is_none(), "{}: twice", kind.name());
        self.0.push((kind, value));
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (IndexKind, &T)> {
        self.0.iter().map(|(kind, value)| (*kind, value))
    }

    pub(crate) fn at(&self, position: usize) -> (IndexKind, &T) {
        let (kind, value) = &self.0[position];
        (*kind, value)
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}
