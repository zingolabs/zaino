//! [`Publisher`]: the latest NFS publish + the latest chain view → one stored [`GrpcSnapshot`]
//!
//! ```text
//!  indexed.changed() ─┐                        ┌─▶ served moved? new epoch : arrivals appended
//!  view published ────┼─▶ compose(prev, both) ─┼─▶ ArcSwap::store(GrpcSnapshot { seq + 1, .. })
//!  cancel ────────────┘                        ├─▶ old epoch sealed (after the store: G5)
//!                                              └─▶ transitions(prev, next)
//! ```
//!
//! - No tearing: each input = one immutable `Arc`; a snapshot = two `Arc`s + `Copy` tips
//! - Inputs `watch`-coalesced: a burst publishes once, from the latest of both

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chainview::{ChainViewSnapshot, ChainViewSubscriber};
use zaino_nfs::{NfsPublication, Published};
use zaino_primitives::types::FinalityDepth;

use crate::compose::compose;
use crate::feed::{above_durable_txids, Feed};
use crate::grpc_snapshot::GrpcSnapshot;
use crate::report::transitions;

#[derive(Debug, thiserror::Error)]
pub enum GrpcSnapshotError {
    #[error("the NFS stopped publishing")]
    IndexedGone,
}

/// Reader's end: one load per request or stream, never `None` (seq 0 stored at `new`)
pub struct GrpcSnapshots<V> {
    current: Arc<ArcSwap<GrpcSnapshot<V>>>,
    changed: watch::Receiver<()>,
}

impl<V> Clone for GrpcSnapshots<V> {
    fn clone(&self) -> Self {
        Self { current: Arc::clone(&self.current), changed: self.changed.clone() }
    }
}

impl<V> GrpcSnapshots<V> {
    pub fn load(&self) -> Arc<GrpcSnapshot<V>> {
        self.current.load_full()
    }

    /// Next publish (`Err` = the publisher stopped)
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.changed.changed().await
    }
}

#[cfg(any(test, feature = "testing"))]
impl<V> GrpcSnapshots<V> {
    /// One snapshot for good, no publisher (consumers' tests)
    ///
    /// - never republished: `changed()` errs, a tail ends after its opening
    pub fn fixed(indexed: Option<Arc<NfsPublication<V>>>, view: Arc<ChainViewSnapshot>) -> Self {
        Core::new(indexed, view, FinalityDepth::CONSENSUS).handle()
    }
}

/// One writer: the stored snapshot + the tails' wake
pub(crate) struct Core<V> {
    depth: FinalityDepth,
    current: Arc<ArcSwap<GrpcSnapshot<V>>>,
    changed: watch::Sender<()>,
    wake: watch::Sender<()>,
}

impl<V> Core<V> {
    /// Seq 0 from the inputs' current values (nothing synced yet)
    pub(crate) fn new(
        indexed: Option<Arc<NfsPublication<V>>>,
        view: Arc<ChainViewSnapshot>,
        depth: FinalityDepth,
    ) -> Self {
        let wake = watch::Sender::new(());
        let tips = compose(false, indexed.as_deref(), &view, depth);
        let mined = above_durable_txids(indexed.as_deref());
        let feed = Feed::open(view.arrivals(None), mined, wake.subscribe());
        let first = GrpcSnapshot { seq: 0, tips, indexed, view, feed };
        let current = Arc::new(ArcSwap::from_pointee(first));
        Self { depth, current, changed: watch::Sender::new(()), wake }
    }

    pub(crate) fn handle(&self) -> GrpcSnapshots<V> {
        GrpcSnapshots { current: Arc::clone(&self.current), changed: self.changed.subscribe() }
    }

    /// One publish: open → store → seal (G5), then wake readers and tails
    pub(crate) fn publish(
        &self,
        indexed: Option<Arc<NfsPublication<V>>>,
        view: Arc<ChainViewSnapshot>,
    ) {
        let prev = self.current.load_full();
        let tips = compose(prev.tips.synced, indexed.as_deref(), &view, self.depth);
        let moved = tips.served != prev.tips.served;
        let feed = match moved {
            true => {
                let mined = above_durable_txids(indexed.as_deref());
                Feed::open(view.arrivals(None), mined, self.wake.subscribe())
            }
            false => {
                view.arrivals(Some(&prev.view))
                    .into_iter()
                    .for_each(|entry| prev.feed.append(entry));
                prev.feed.clone()
            }
        };
        let next = Arc::new(GrpcSnapshot { seq: prev.seq + 1, tips, indexed, view, feed });
        self.current.store(Arc::clone(&next));
        if moved {
            prev.feed.seal();
        }
        self.changed.send_replace(());
        self.wake.send_replace(());
        transitions(&prev, &next);
    }
}

/// `Core` fed by the NFS's publication watch and the chain view's (its only inputs)
pub struct Publisher<V> {
    core: Core<V>,
    indexed: Published<V>,
    view: ChainViewSubscriber,
    published: watch::Receiver<()>,
}

impl<V> Publisher<V> {
    /// - `depth` = the header chain's reorg depth (`synced` closes past it)
    /// - stores seq 0 at once
    pub fn new(mut indexed: Published<V>, view: ChainViewSubscriber, depth: FinalityDepth) -> Self {
        let mut published = view.subscribe_published();
        published.borrow_and_update();
        let latest = indexed.borrow_and_update().clone();
        let core = Core::new(latest, view.current(), depth);
        Self { core, indexed, view, published }
    }

    pub fn handle(&self) -> GrpcSnapshots<V> {
        self.core.handle()
    }

    /// Cancel → `Ok`; the NFS gone before it → `IndexedGone`
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), GrpcSnapshotError> {
        loop {
            tokio::select! {
                // cancel first (the NFS stops on the same cancel)
                biased;
                () = cancel.cancelled() => return Ok(()),
                changed = self.indexed.changed() => {
                    changed.map_err(|_| GrpcSnapshotError::IndexedGone)?;
                }
                // sender held by the view's core, alive while `self.view` is
                _ = self.published.changed() => {}
            }
            self.published.borrow_and_update();
            let indexed = self.indexed.borrow_and_update().clone();
            self.core.publish(indexed, self.view.current());
        }
    }
}
