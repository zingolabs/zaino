use std::hash::Hash;

use imbl::{
    shared_ptr::{DefaultSharedPtr, SharedPointer, SharedPointerKind},
    vector, GenericVector,
};

#[derive(Debug)]
pub(crate) struct ImblNonEmptyVec<T, P = DefaultSharedPtr>
where
    P: SharedPointerKind,
{
    head: SharedPointer<T, P>,
    tail: GenericVector<T, P>,
}

impl<T> ImblNonEmptyVec<T> {
    pub(crate) fn new(initial: T) -> Self {
        Self {
            head: SharedPointer::new(initial),
            tail: vector![],
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(self.head.as_ref()).chain(self.tail.iter())
    }

    pub(crate) fn last(&self) -> &T {
        self.tail.last().unwrap_or(&self.head)
    }
}

impl<T: Clone> Clone for ImblNonEmptyVec<T> {
    fn clone(&self) -> Self {
        Self {
            head: self.head.clone(),
            tail: self.tail.clone(),
        }
    }
}

impl<T: PartialEq> PartialEq for ImblNonEmptyVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.head == other.head && self.tail == other.tail
    }
}
impl<T: Eq> Eq for ImblNonEmptyVec<T> {}
impl<T: Hash> Hash for ImblNonEmptyVec<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.head.hash(state);
        self.tail.hash(state);
    }
}
