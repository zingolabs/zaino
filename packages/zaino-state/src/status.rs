//! Thread-safe status wrapper.
//!
//! This module provides [`AtomicStatus`], a thread-safe wrapper for [`StatusType`],
//! and [`NamedAtomicStatus`], a variant that logs status transitions and supports
//! awaiting transitions via [`NamedAtomicStatus::subscribe`].

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use tokio::sync::watch;
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
/// Backed by a [`tokio::sync::watch`] channel: every transition is logged and
/// every subscriber wakes via [`watch::Receiver::changed`]. Identical-value
/// stores are no-ops — neither logged nor broadcast.
#[derive(Debug, Clone)]
pub struct NamedAtomicStatus {
    name: &'static str,
    inner: Arc<watch::Sender<StatusType>>,
}

impl NamedAtomicStatus {
    /// Creates a new NamedAtomicStatus with the given component name and initial status.
    pub fn new(name: &'static str, status: StatusType) -> Self {
        debug!(component = name, status = %status, "[STATUS] initial");
        let (tx, _rx) = watch::channel(status);
        Self {
            name,
            inner: Arc::new(tx),
        }
    }

    /// Returns the component name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Loads the value held in the NamedAtomicStatus.
    pub fn load(&self) -> StatusType {
        *self.inner.borrow()
    }

    /// Sets the value held in the NamedAtomicStatus, logging and broadcasting
    /// the transition. Storing the current value is a no-op.
    pub fn store(&self, status: StatusType) {
        let old = self.load();
        if old != status {
            debug!(
                component = self.name,
                from = %old,
                to = %status,
                "[STATUS] transition"
            );
            self.inner.send_replace(status);
        }
    }

    /// Returns a [`watch::Receiver`] that observes every status transition.
    ///
    /// Use this to wait for a specific state with
    /// [`watch::Receiver::changed`] / [`watch::Receiver::borrow_and_update`]
    /// instead of busy-polling [`Self::load`].
    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> watch::Receiver<StatusType> {
        self.inner.subscribe()
    }
}

impl Status for NamedAtomicStatus {
    fn status(&self) -> StatusType {
        self.load()
    }
}
