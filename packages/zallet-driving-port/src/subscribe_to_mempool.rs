//! Capability: subscribe to the mempool.

use futures_core::Stream;

use crate::mempool_transaction::MempoolTransaction;

/// Subscribe to the mempool.
///
/// The mempool stands apart from chain state (ADR 0001): this stream
/// **survives tip changes** — it never ends to signal one, retiring
/// the closes-on-tip-change idiom. Drivers learn of chain movement
/// from [`crate::SubscribeToTipChanges`] and compose the two,
/// resubscribing here when they want the mempool as revalidated
/// against a new tip.
///
/// Semantics: a fresh subscription delivers the current mempool
/// contents first, then arrivals as the engine accepts them; every
/// delivery is tagged with the tip it was validated against. The
/// stream carries arrivals only — it never signals removals; the
/// stream ends only when the port shuts down. A driver's accumulated
/// mempool view is therefore a superset of the engine's mempool: the
/// engine may evict a transaction between tip events with no signal,
/// and the view is trued up by resubscribing. Mempool presence is a
/// hint, never authoritative.
pub trait SubscribeToMempool: Send + Sync {
    /// Mempool transactions: the current contents, then arrivals.
    fn subscribe_to_mempool(&self) -> impl Stream<Item = MempoolTransaction> + Send;
}
