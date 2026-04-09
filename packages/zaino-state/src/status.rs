//! Thread-safe status wrapper.
//!
//! This module provides [`AtomicStatus`], a thread-safe wrapper for [`StatusType`],
//! and [`NamedAtomicStatus`], a variant that logs status transitions.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use tracing::debug;

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

/// Thread-safe status wrapper with component name for observability.
///
/// Unlike [`AtomicStatus`], this variant logs all status transitions,
/// making it easier to trace component lifecycle during debugging.
#[derive(Debug, Clone)]
pub struct NamedAtomicStatus {
    name: &'static str,
    inner: Arc<AtomicUsize>,
}

impl NamedAtomicStatus {
    /// Creates a new NamedAtomicStatus with the given component name and initial status.
    pub fn new(name: &'static str, status: StatusType) -> Self {
        debug!(component = name, status = %status, "[STATUS] initial");
        Self {
            name,
            inner: Arc::new(AtomicUsize::new(status.into())),
        }
    }

    /// Returns the component name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Loads the value held in the NamedAtomicStatus.
    pub fn load(&self) -> StatusType {
        StatusType::from(self.inner.load(Ordering::SeqCst))
    }

    /// Sets the value held in the NamedAtomicStatus, logging the transition.
    pub fn store(&self, status: StatusType) {
        let old = self.load();
        if old != status {
            debug!(
                component = self.name,
                from = %old,
                to = %status,
                "[STATUS] transition"
            );
        }
        self.inner.store(status.into(), Ordering::SeqCst);
    }
}

impl Status for NamedAtomicStatus {
    fn status(&self) -> StatusType {
        self.load()
    }
}
