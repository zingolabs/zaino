//! Thread-safe status wrapper.
//!
//! This module provides [`AtomicStatus`], a thread-safe wrapper for [`StatusType`].

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub use zaino_common::status::{Status, StatusType};

/// Holds a thread-safe representation of a [`StatusType`].
#[derive(Debug, Clone)]
pub struct AtomicStatus {
    inner: Arc<AtomicUsize>,
}

impl AtomicStatus {
    /// Creates a new AtomicStatus.
    pub fn new(status: StatusType) -> Self {
        Self {
            inner: Arc::new(AtomicUsize::new(status.into())),
        }
    }

    /// Loads the value held in the AtomicStatus.
    pub fn load(&self) -> StatusType {
        StatusType::from(self.inner.load(Ordering::SeqCst))
    }

    /// Sets the value held in the AtomicStatus.
    pub fn store(&self, status: StatusType) {
        self.inner.store(status.into(), Ordering::SeqCst);
    }
}

impl Status for AtomicStatus {
    fn status(&self) -> StatusType {
        self.load()
    }
}
