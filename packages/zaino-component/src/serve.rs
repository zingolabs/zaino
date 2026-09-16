//! A supervised long-running server — the seam supervision drives.

use core::future::Future;
use std::sync::Arc;

use crate::CancellationToken;

/// Fired by a server once it has **bound and is about to serve**, so the
/// component reports `Ready` only when the socket is actually accepting — not
/// optimistically at spawn. If the server returns `Err` before firing it, the
/// component never becomes `Ready` (a boot failure), which is the honest outcome.
pub struct ReadySignal {
    on_ready: Box<dyn FnOnce() + Send>,
}

impl ReadySignal {
    /// A signal that runs `on_ready` when the server reports it has bound.
    pub fn new(on_ready: impl FnOnce() + Send + 'static) -> Self {
        Self {
            on_ready: Box::new(on_ready),
        }
    }

    /// Report that the server is bound and serving.
    pub fn notify(self) {
        (self.on_ready)()
    }
}

/// A long-running server the runtime supervises.
///
/// `serve` binds, fires `ready` once bound, and runs until `cancel` fires.
/// Returning `Ok(())` means a clean shutdown in response to the token; returning
/// `Err` means the server could not bind or its serve loop failed — which a
/// supervisor turns into a `Critical` component (and, if `ready` never fired, a
/// boot failure). A concrete transport server (tonic / jsonrpsee) implements
/// this over its profile handle.
///
/// The receiver is `Arc<Self>` so the server can be shared with the spawned
/// serve task while the component retains a handle.
pub trait Serve: Send + Sync + 'static {
    /// Why the server could not start or keep running.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Bind, fire `ready` once bound, and serve until `cancel` fires.
    fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
