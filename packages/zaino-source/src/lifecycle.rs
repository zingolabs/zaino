//! Lifecycle: releasing resources a source owns.

/// Release any long-lived resources the source holds.
///
/// Separate from the query traits because it is not a question about the
/// chain: it is about the adapter itself. Most adapters own nothing beyond a
/// connection pool and inherit the no-op default; an adapter that drives its
/// own background work — a syncer task, a spawned service — overrides it so
/// that work cannot outlive the indexer.
///
/// Synchronous and infallible by design. It is called from teardown paths that
/// may have no runtime to await on, including `Drop`, and a shutdown that can
/// fail gives the caller no useful recourse at that point.
pub trait SourceLifecycle: Send + Sync {
    /// Release owned resources. Idempotent: calling it twice is not an error.
    fn shutdown(&self) {}
}
