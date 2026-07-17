//! Capability: shut the port down.

use std::future::Future;

/// Shut the port down.
///
/// Idempotent. Once the returned future resolves, every subscription
/// stream ends and subsequent operations fail with a fatal backend
/// error. Snapshots already handed out are not revoked — their clones
/// may outlive the port's serving surface, but the strong pinning
/// guarantee only promises data while the port lives.
pub trait ShutDown: Send + Sync {
    /// Stop serving and end all subscription streams.
    fn shut_down(&self) -> impl Future<Output = ()> + Send;
}
