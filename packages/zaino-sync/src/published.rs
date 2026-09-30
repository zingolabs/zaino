//! What an index loop publishes for serving, metrics and status: its view, its two tips, and
//! (judged by its own task, off the loop) the serving gate

use std::{future::Future, sync::Arc, time::Instant};

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zaino_chainview::QuorumTip;
use zaino_primitives::types::{Height, ReorgDepth};

use crate::{report::Human, Reads, Served};

/// One index's published state (the loop writes it, everything else reads it)
///
/// - `synced` = the serving gate, written only by [`gate`](Self::gate)'s task
/// - `reorgs` = bumped per reorg: the gate closes until the replay is back at the tip
pub struct Published<V> {
    view: Arc<ArcSwap<V>>,
    applied: watch::Sender<Option<Height>>,
    finalized: watch::Sender<Option<Height>>,
    reorgs: watch::Sender<u64>,
    synced: Arc<watch::Sender<bool>>,
    reads: Reads,
}

impl<V> Published<V> {
    /// At boot: `view` over durable state alone, both tips at `durable`
    pub fn new(view: V, durable: Option<Height>) -> Self {
        Self {
            view: Arc::new(ArcSwap::from_pointee(view)),
            applied: watch::Sender::new(durable),
            finalized: watch::Sender::new(durable),
            reorgs: watch::Sender::new(0),
            synced: Arc::new(watch::Sender::new(false)),
            reads: Reads::default(),
        }
    }

    /// View + applied height, together (readers pin both tiers from one publication)
    pub fn view(&self, view: V, applied: Option<Height>) {
        self.view.store(Arc::new(view));
        self.applied.send_if_modified(|current| std::mem::replace(current, applied) != applied);
    }

    /// Durable tip after a landed write (on disk *before* published), after the landing's
    /// [`view`](Self::view) (a reader woken here pins the view holding it)
    pub fn durable(&self, finalized: Option<Height>) {
        let before = self.finalized.send_replace(finalized);
        assert!(before <= finalized, "durable tip moved back");
    }

    /// Non-finalized state dropped: the gate closes until the replay reaches the tip again
    ///
    /// - After the dropped [`view`](Self::view) (a gate reading the old applied tip would reopen)
    pub fn reorged(&self) {
        self.reorgs.send_modify(|reorgs| *reorgs += 1);
    }

    /// What this index's services read, gated on `synced`
    pub fn served(&self) -> Served<V> {
        Served::counted(Arc::clone(&self.view), self.synced.subscribe(), self.reads.clone())
    }

    pub fn reads(&self) -> Reads {
        self.reads.clone()
    }

    pub fn subscribe_finalized(&self) -> watch::Receiver<Option<Height>> {
        self.finalized.subscribe()
    }

    pub fn subscribe_applied(&self) -> watch::Receiver<Option<Height>> {
        self.applied.subscribe()
    }

    pub fn subscribe_synced(&self) -> watch::Receiver<bool> {
        self.synced.subscribe()
    }

    /// The serving gate as its own task, until `cancel`
    ///
    /// - Opens once applied reaches chainview's quorum tip
    /// - Closes more than `depth` behind it (producer stalled), or on a reorg until its replay is
    ///   back at the tip
    pub fn gate(
        &self,
        mut tips: watch::Receiver<Option<QuorumTip>>,
        depth: ReorgDepth,
        cancel: CancellationToken,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (mut applied, mut reorgs) = (self.applied.subscribe(), self.reorgs.subscribe());
        let synced = Arc::clone(&self.synced);
        async move {
            let mut replaying: Option<Instant> = None;
            // counted, not `has_changed`: the `changed()` that woke the loop already marked it seen
            let mut reorgs_seen = *reorgs.borrow_and_update();
            loop {
                let tip = tips.borrow_and_update().as_ref().map(|tip| tip.block.height);
                let applied_now = *applied.borrow_and_update();
                let reorged = *reorgs.borrow_and_update();
                if reorged != reorgs_seen {
                    reorgs_seen = reorged;
                    replaying = Some(Instant::now());
                    set(&synced, false, applied_now, None);
                }
                let at_tip = tip.is_some() && tip <= applied_now;
                let open = match *synced.borrow() {
                    false => at_tip,
                    true => tip.is_none_or(|tip| {
                        u32::from(tip).saturating_sub(applied_now.map_or(0, u32::from))
                            <= depth.get()
                    }),
                };
                if set(&synced, open, applied_now, replaying) && open {
                    replaying = None;
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    moved = tips.changed() => if moved.is_err() { return },
                    moved = applied.changed() => if moved.is_err() { return },
                    moved = reorgs.changed() => if moved.is_err() { return },
                }
            }
        }
    }
}

/// Gate set; `true` = it changed (logged, a reorg's replay timed from its reset)
fn set(
    synced: &watch::Sender<bool>,
    open: bool,
    applied: Option<Height>,
    replaying: Option<Instant>,
) -> bool {
    let changed = synced.send_if_modified(|gate| std::mem::replace(gate, open) != open);
    if changed {
        let height = applied.map_or(0, u32::from);
        match (open, replaying) {
            (true, Some(reset)) => {
                info!(height, took = %Human(reset.elapsed()), "Reorg replayed, serving")
            }
            (true, None) => info!(height, "Serving"),
            (false, Some(_)) => info!(height, "Reorg received, requests refused until replayed"),
            (false, None) => info!(height, "Syncing, requests refused"),
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zaino_primitives::types::{BlockHash, BlockRef};

    use super::*;

    fn h(n: u32) -> Option<Height> {
        Some(Height::try_from(n).expect("h"))
    }

    fn quorum(height: u32) -> Option<QuorumTip> {
        let block = BlockRef { hash: BlockHash::from([0; 32]), height: h(height).expect("h") };
        Some(QuorumTip { block, agreed_by: zaino_chainview::EndpointSet::default() })
    }

    /// Depth 3: closed below the tip, open once there, stays open within the depth, closes past
    /// it (producer stalled), closes on a reorg until the replay is back at the tip
    #[tokio::test]
    async fn the_gate_opens_at_the_tip_holds_within_the_depth_and_closes_on_a_stall_or_a_reorg() {
        let published = Published::new((), h(10));
        let (tips, rx) = watch::channel(quorum(20));
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(3).expect("non-zero"));
        let cancel = CancellationToken::new();
        let gate = tokio::spawn(published.gate(rx, depth, cancel.clone()));
        let mut synced = published.subscribe_synced();
        let within = Duration::from_secs(5);
        let wait = |synced: &mut watch::Receiver<bool>, open: bool| {
            let mut synced = synced.clone();
            async move {
                tokio::time::timeout(within, synced.wait_for(|now| *now == open))
                    .await
                    .unwrap_or_else(|_| {
                        panic!("gate never {}", if open { "opened" } else { "closed" })
                    })
                    .map(|_| ())
                    .expect("gate alive")
            }
        };

        tokio::task::yield_now().await;
        assert!(!*synced.borrow(), "10 applied, tip 20: syncing");
        published.view((), h(20));
        wait(&mut synced, true).await;
        tips.send_replace(quorum(23));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(*synced.borrow(), "3 behind = within the depth: no flap");
        tips.send_replace(quorum(24));
        wait(&mut synced, false).await;
        published.view((), h(24));
        wait(&mut synced, true).await;

        published.view((), h(21));
        published.reorged();
        wait(&mut synced, false).await;
        published.view((), h(24));
        wait(&mut synced, true).await;

        cancel.cancel();
        gate.await.expect("gate task ends on cancel");
    }
}
