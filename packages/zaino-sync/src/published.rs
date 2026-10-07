//! What an index loop publishes for serving, metrics and status: its view, its two tips, and
//! (judged by its own task, off the loop) the serving gate

use std::{future::Future, sync::Arc, time::Instant};

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{BlockRef, Height, ReorgDepth};

use crate::{report::Human, Reads, Served};

/// One index's published state (the loop writes it, everything else reads it)
///
/// - `synced` = the serving gate, written only by [`gate`](Self::gate)'s task
/// - `reorgs` = bumped per reorg: the gate closes until the replay is back at the tip
/// - `merged` = last final block held for the next bulk commit (`None` once a commit covers it)
pub struct Published<V> {
    view: Arc<ArcSwap<V>>,
    applied: watch::Sender<Option<BlockRef>>,
    finalized: watch::Sender<Option<Height>>,
    merged: watch::Sender<Option<Height>>,
    reorgs: watch::Sender<u64>,
    synced: Arc<watch::Sender<bool>>,
    reads: Reads,
}

impl<V> Published<V> {
    /// At boot: `view` over durable state alone, both tips at `durable`
    pub fn new(view: V, durable: Option<BlockRef>) -> Self {
        Self {
            view: Arc::new(ArcSwap::from_pointee(view)),
            applied: watch::Sender::new(durable),
            finalized: watch::Sender::new(durable.map(|tip| tip.height)),
            merged: watch::Sender::new(None),
            reorgs: watch::Sender::new(0),
            synced: Arc::new(watch::Sender::new(false)),
            reads: Reads::default(),
        }
    }

    /// View + applied block, together (readers pin both tiers from one publication)
    pub fn view(&self, view: V, applied: Option<BlockRef>) {
        self.view.store(Arc::new(view));
        self.applied.send_if_modified(|current| std::mem::replace(current, applied) != applied);
    }

    /// Durable tip after a landed write (on disk *before* published), after the landing's
    /// [`view`](Self::view) (a reader woken here pins the view holding it)
    pub fn durable(&self, finalized: Option<Height>) {
        let before = self.finalized.send_replace(finalized);
        assert!(before <= finalized, "durable tip moved back");
        self.merged.send_if_modified(|merged| {
            let landed = merged.is_some_and(|merged| Some(merged) <= finalized);
            if landed {
                *merged = None;
            }
            landed
        });
    }

    /// Final block `height` held in memory for the next bulk commit (progress between commits)
    pub fn merged(&self, height: Height) {
        self.merged.send_replace(Some(height));
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

    pub fn subscribe_applied(&self) -> watch::Receiver<Option<BlockRef>> {
        self.applied.subscribe()
    }

    pub fn subscribe_merged(&self) -> watch::Receiver<Option<Height>> {
        self.merged.subscribe()
    }

    pub fn subscribe_synced(&self) -> watch::Receiver<bool> {
        self.synced.subscribe()
    }

    /// The serving gate as its own task, until `cancel`
    ///
    /// - Opens once the applied block **is** the verified best (hash, not height)
    /// - Closes once the applied block leaves the best chain, falls more than `depth` behind it
    ///   (producer stalled), or on a reorg until its replay is back at the best
    pub fn gate(
        &self,
        mut chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
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
                let verified = chain.borrow_and_update().clone();
                let applied_now = *applied.borrow_and_update();
                let reorged = *reorgs.borrow_and_update();
                if reorged != reorgs_seen {
                    reorgs_seen = reorged;
                    replaying = Some(Instant::now());
                    set(&synced, false, applied_now, None);
                }
                let open = match (verified, applied_now) {
                    // nothing verified yet (boot): unchanged
                    (None, _) => *synced.borrow(),
                    (Some(_), None) => false,
                    (Some(chain), Some(applied)) => {
                        let best = chain.best();
                        let on_best = chain.hash_at(applied.height) == Some(applied.hash);
                        let behind = u32::from(best.height).saturating_sub(applied.height.into());
                        match *synced.borrow() {
                            false => applied == best,
                            true => on_best && behind <= depth.get(),
                        }
                    }
                };
                if set(&synced, open, applied_now, replaying) && open {
                    replaying = None;
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    moved = chain.changed() => if moved.is_err() { return },
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
    applied: Option<BlockRef>,
    replaying: Option<Instant>,
) -> bool {
    let changed = synced.send_if_modified(|gate| std::mem::replace(gate, open) != open);
    if changed {
        let height = applied.map_or(0, |applied| u32::from(applied.height));
        match (open, replaying) {
            (true, Some(reset)) => {
                info!(height, took = %Human(reset.elapsed()), "Reorg replayed, serving")
            }
            // INFO line = zainod's index report (heights + size)
            (true, None) => debug!(height, "Serving"),
            (false, Some(_)) => info!(height, "Reorg received, requests refused until replayed"),
            (false, None) => info!(height, "Syncing, requests refused"),
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::Block;

    use super::*;

    /// Depth 3, chain A 0..=24, B22 forking after A21 (heavier than A22..=A24): closed below the
    /// best, open once the applied block is it, open within the depth, closed past it (producer
    /// stalled), on a reorg until the replay is back, and once the best moves off the applied
    /// block's branch (no reorg step needed: hash, not height)
    #[tokio::test(start_paused = true)]
    async fn the_gate_opens_on_the_best_block_and_closes_on_a_stall_a_reorg_or_a_branch_change() {
        let mut chain = Chain::new();
        let a24 = chain.extend(chain.genesis().hash, 24);
        let a: Vec<Block> = chain.path(a24.hash);
        let b22 = chain.mine_bits(a[21].header().hash, a[22].header().time, 0x1f0f_0f0f);
        let at = |block: &Block| {
            Some(BlockRef { hash: block.header().hash, height: block.header().height })
        };
        let best = |tip: BlockRef| Some(Arc::new(VerifiedChain::regtest(&chain.path(tip.hash))));
        let a_best = |h: usize| best(at(&a[h]).expect("block"));
        let published = Published::new((), at(&a[10]));
        let (tips, rx) = watch::channel(a_best(20));
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
        assert!(!*synced.borrow(), "10 applied, best 20: syncing");
        published.view((), at(&a[20]));
        wait(&mut synced, true).await;
        tips.send_replace(a_best(23));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(*synced.borrow(), "3 behind = within the depth: no flap");
        tips.send_replace(a_best(24));
        wait(&mut synced, false).await;
        published.view((), at(&a[24]));
        wait(&mut synced, true).await;

        published.view((), at(&a[21]));
        published.reorged();
        wait(&mut synced, false).await;
        published.view((), at(&a[24]));
        wait(&mut synced, true).await;

        tips.send_replace(best(BlockRef { hash: b22.hash, height: b22.height }));
        wait(&mut synced, false).await;
        published.view((), at(&a[21]));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!*synced.borrow(), "A21 on B's chain, 1 behind: still replaying");
        published.view((), at(chain.block(b22.hash)));
        wait(&mut synced, true).await;

        cancel.cancel();
        gate.await.expect("gate task ends on cancel");
    }
}
