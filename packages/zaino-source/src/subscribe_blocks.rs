//! Subscription: wake on new blocks at the source.

use tokio::sync::watch;

/// Subscribe to "the source has new blocks" notifications.
///
/// A latency hint and nothing more. The channel carries `()`, so a subscriber
/// learns only *that* the source advanced, never how far — it must re-read the
/// source's state on each wake. Correctness therefore never depends on
/// receiving a notification, and a consumer that ignores this trait entirely is
/// still correct, just slower.
///
/// [`watch`] rather than a queue, deliberately: it coalesces by construction,
/// so any number of sends between two receives collapse into one wake. A
/// consumer that re-reads state on every wake wants exactly that, and cannot
/// fall behind a burst of blocks.
///
/// Returning `None` means "no push path available" — poll-only adapters answer
/// that way and their consumers pace themselves on a timer.
pub trait SubscribeBlocks: Send + Sync {
    /// Subscribe to block-arrival wakes, if this source offers a push path.
    fn subscribe_to_blocks_received(&self) -> Option<watch::Receiver<()>> {
        None
    }
}
