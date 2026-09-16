//! The sync seam — a driven port for index-building, driven by a supervised
//! indexer component.

use core::future::Future;
use std::sync::Arc;

use crate::{CancellationToken, ReadySignal};

/// Drives index-building: sources blocks and writes the index.
///
/// `run` starts syncing, fires `caught_up` the first time it reaches the tip,
/// and follows the chain until `cancel`. `Ok(())` is a clean stop; `Err` is a
/// source/write failure — which a supervisor turns into a `Critical` component.
///
/// This is a *driven* port: the concrete sync engine (e.g. ChainView's sync)
/// implements it, and the runtime's indexer component drives it. It lives here,
/// with the component vocabulary, so a domain crate can implement it depending
/// only on `zaino-component`, never on the runtime.
pub trait SyncDriver: Send + Sync + 'static {
    /// Why syncing could not continue.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Build the index until cancelled, firing `caught_up` once at the tip.
    fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        caught_up: ReadySignal,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
