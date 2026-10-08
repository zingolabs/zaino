//! [`Publisher`]: the latest NFS publish + the latest chain view → one stored [`Snapshot`]
//!
//! ```text
//!  indexed.changed() ─┐                        ┌─▶ served moved? new epoch : arrivals appended
//!  view published ────┼─▶ compose(prev, both) ─┼─▶ ArcSwap::store(Snapshot { seq + 1, .. })
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
use zaino_nfs::{Indexed, Published};
use zaino_primitives::types::ReorgDepth;

use crate::compose::{check, compose};
use crate::feed::Feed;
use crate::report::transitions;
use crate::snapshot::Snapshot;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("the NFS stopped publishing")]
    IndexedGone,
}

/// Reader's end: one load per request or stream, never `None` (seq 0 stored at `new`)
pub struct Snapshots<V> {
    current: Arc<ArcSwap<Snapshot<V>>>,
    changed: watch::Receiver<()>,
}

impl<V> Clone for Snapshots<V> {
    fn clone(&self) -> Self {
        Self { current: Arc::clone(&self.current), changed: self.changed.clone() }
    }
}

impl<V> Snapshots<V> {
    pub fn load(&self) -> Arc<Snapshot<V>> {
        self.current.load_full()
    }

    /// Next publish (`Err` = the publisher stopped)
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.changed.changed().await
    }
}

#[cfg(any(test, feature = "testing"))]
impl<V> Snapshots<V> {
    /// One snapshot for good, no publisher (consumers' tests)
    ///
    /// - never republished: `changed()` errs, a tail ends after its opening
    pub fn fixed(indexed: Option<Arc<Indexed<V>>>, view: Arc<ChainViewSnapshot>) -> Self {
        Core::new(indexed, view, ReorgDepth::CONSENSUS).handle()
    }
}

/// One writer: the stored snapshot + the tails' wake
pub(crate) struct Core<V> {
    depth: ReorgDepth,
    current: Arc<ArcSwap<Snapshot<V>>>,
    changed: watch::Sender<()>,
    wake: watch::Sender<()>,
}

impl<V> Core<V> {
    /// Seq 0 from the inputs' current values (nothing synced yet)
    pub(crate) fn new(
        indexed: Option<Arc<Indexed<V>>>,
        view: Arc<ChainViewSnapshot>,
        depth: ReorgDepth,
    ) -> Self {
        let wake = watch::Sender::new(());
        let tips = compose(false, indexed.as_deref(), &view, depth);
        let feed = Feed::open(tips.served, view.arrivals(None), wake.subscribe());
        let first = Snapshot { seq: 0, tips, indexed, view, feed };
        let current = Arc::new(ArcSwap::from_pointee(first));
        Self { depth, current, changed: watch::Sender::new(()), wake }
    }

    pub(crate) fn handle(&self) -> Snapshots<V> {
        Snapshots { current: Arc::clone(&self.current), changed: self.changed.subscribe() }
    }

    /// One publish: open → store → seal (G5), then wake readers and tails
    pub(crate) fn publish(&self, indexed: Option<Arc<Indexed<V>>>, view: Arc<ChainViewSnapshot>) {
        let prev = self.current.load_full();
        let tips = compose(prev.tips.synced, indexed.as_deref(), &view, self.depth);
        let moved = tips.served != prev.tips.served;
        let feed = match moved {
            true => Feed::open(tips.served, view.arrivals(None), self.wake.subscribe()),
            false => {
                view.arrivals(Some(&prev.view))
                    .into_iter()
                    .for_each(|entry| prev.feed.append(entry));
                prev.feed.clone()
            }
        };
        let next = Arc::new(Snapshot { seq: prev.seq + 1, tips, indexed, view, feed });
        self.current.store(Arc::clone(&next));
        if moved {
            prev.feed.seal();
        }
        self.changed.send_replace(());
        self.wake.send_replace(());
        transitions(&prev, &next);
        if cfg!(debug_assertions) {
            check(&prev, &next, self.depth);
        }
    }
}

/// `Core` fed by the NFS's publication watch and the chain view's (I4: its only inputs)
pub struct Publisher<V> {
    core: Core<V>,
    indexed: Published<V>,
    view: ChainViewSubscriber,
    published: watch::Receiver<()>,
}

impl<V> Publisher<V> {
    /// - `depth` = the header chain's reorg depth (`synced` closes past it)
    /// - stores seq 0 at once
    pub fn new(mut indexed: Published<V>, view: ChainViewSubscriber, depth: ReorgDepth) -> Self {
        let mut published = view.subscribe_published();
        published.borrow_and_update();
        let latest = indexed.borrow_and_update().clone();
        let core = Core::new(latest, view.current(), depth);
        Self { core, indexed, view, published }
    }

    pub fn handle(&self) -> Snapshots<V> {
        self.core.handle()
    }

    /// Cancel → `Ok`; the NFS gone → `IndexedGone`
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), SnapshotError> {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                changed = self.indexed.changed() => {
                    changed.map_err(|_| SnapshotError::IndexedGone)?;
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
