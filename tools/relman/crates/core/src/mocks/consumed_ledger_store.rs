use std::sync::Mutex;

use crate::ports::{ConsumedLedgerStore, ConsumedLedgerStoreError};
use crate::types::ConsumedLedger;

/// An in-memory [`ConsumedLedgerStore`] backed by a single [`ConsumedLedger`].
///
/// Makes domain tests deterministic and I/O-free: `read` clones the held ledger,
/// `write` replaces it. Starts empty, mirroring a missing file on disk. Interior
/// mutability via a `Mutex` keeps the port's `&self` signature (the real fs
/// adapter is likewise shared).
#[derive(Default)]
pub struct MapConsumedLedgerStore {
    ledger: Mutex<ConsumedLedger>,
}

impl MapConsumedLedgerStore {
    /// An empty store (an absent ledger).
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the store with an already-populated ledger, to exercise the
    /// exclude-by-id path without going through `write` first.
    pub fn with_ledger(ledger: ConsumedLedger) -> Self {
        Self {
            ledger: Mutex::new(ledger),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ConsumedLedger> {
        self.ledger
            .lock()
            .expect("MapConsumedLedgerStore mutex poisoned")
    }
}

impl ConsumedLedgerStore for MapConsumedLedgerStore {
    fn read(&self) -> Result<ConsumedLedger, ConsumedLedgerStoreError> {
        Ok(self.lock().clone())
    }

    fn write(&self, ledger: &ConsumedLedger) -> Result<(), ConsumedLedgerStoreError> {
        *self.lock() = ledger.clone();
        Ok(())
    }
}
