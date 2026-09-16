//! A supervised long-running server — the seam supervision drives.

use core::future::Future;
use std::sync::Arc;

use crate::CancellationToken;

/// A long-running server the runtime supervises.
///
/// `serve` binds and runs until `cancel` fires. Returning `Ok(())` means a clean
/// shutdown in response to the token; returning `Err` means the server could not
/// start or its serve loop failed — which a supervisor turns into a `Critical`
/// component. A concrete transport server (tonic / jsonrpsee) implements this
/// over its profile handle.
///
/// The receiver is `Arc<Self>` so the server can be shared with the spawned
/// serve task while the component retains a handle.
pub trait Serve: Send + Sync + 'static {
    /// Why the server could not start or keep running.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Bind and serve until `cancel` fires.
    fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
