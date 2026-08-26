//! The ChainHead runtime.
//!
//! The graph advancement, reorg handling and retention below are the
//! non-finalised state's, moved here and changed only where the new boundaries
//! force it:
//!
//! - the finalised state is gone. The old `sync` took an `Arc<FinalisedState>`
//!   and read `db_height()` for both its anchor floor and its trim floor; both
//!   now come from the chain tip and the configured depth, which is the arm the
//!   old code already took whenever the database lagged.
//! - the source is the `zaino-source` ports rather than the wire-typed
//!   scaffolding, so `get_block(HashOrHeight)` splits into the by-height and
//!   by-hash questions it always was underneath.
//! - blocks are [`ChainHeadBlock`] rather than `IndexedBlock`, so the graph
//!   holds no persistence type.
//! - the runtime owns the loop that used to live in ChainIndex's sync worker.
//!
//! The block-carrying listener and `add_nonbest_block` are not here: no source
//! ever implemented `nonfinalized_listener`, so both were unreachable.
//!
//! Everything else — extending one block at a time, the recursive reorg walk,
//! the non-higher reorg check, best-block selection by accumulated work, and
//! trimming with its keep-the-highest rule — is as it was.
//!
//! # Advancing is not an operation
//!
//! There is no `sync`, `update` or `reconcile` here at any visibility. The
//! writer task is the only thing that advances the graph, and it does so
//! through private methods that build a *new* snapshot and hand it to
//! [`publish_snapshot`](ChainHeadService::publish_snapshot). Nothing else can
//! reach the published cell, so a reader can never observe a half-applied
//! reorg or a partially-extended window.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use arc_swap::ArcSwap;
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use zaino_chain_head::{
    ChainHeadBlock, ChainHeadBlockSource, ChainHeadConfig, ChainHeadSnapshot as _, ChainHeadWork,
};
use zaino_primitives::types::{BlockHash, BlockRef, ChainStateEpoch, Height, TreeRoots};
use zaino_status::{NamedAtomicStatus, Status, StatusType};

use crate::{
    error::{ChainHeadAdvanceError, ChainHeadInitError},
    snapshot::MapBackedSnapshot,
    subscriber::ChainHeadSubscriber,
};

/// The name this component reports status under.
const COMPONENT: &str = "ChainHead";

/// Retention margin below the configured depth.
///
/// Trimming stops this far below the tip rather than exactly at the configured
/// depth, so it never cuts inside the reorg-possible range. It also bounds the
/// reorg ancestry walk: that walk should never recurse further back than the
/// window it maintains.
const RETENTION_MARGIN: u32 = 10;

/// How many frozen blocks the handoff channel buffers before a slow consumer
/// starts missing them.
///
/// A consumer keeping up needs one slot; this leaves room for a store that
/// pauses briefly without it having to rebuild the gap. Beyond that it learns
/// it lagged and rebuilds, which it can always do.
const FROZEN_CHANNEL_CAPACITY: usize = 256;

/// The bounded non-finalised head of the chain, kept current with a validator.
///
/// Owns exactly one writer task. Everything else holds a
/// [`ChainHeadSubscriber`], which reads published snapshots and nothing else.
pub struct ChainHeadService<S: ChainHeadBlockSource> {
    /// We need access to the validator's best block hash, as well as a source
    /// of blocks.
    source: Arc<S>,
    /// This lock should not be exposed to consumers. Rather, clone the Arc and
    /// offer that. This means we can overwrite the arc without interfering with
    /// readers, who will hold a stale copy.
    current: Arc<ArcSwap<MapBackedSnapshot>>,
    updates: watch::Sender<ChainStateEpoch>,
    frozen: broadcast::Sender<ChainHeadBlock>,
    status: NamedAtomicStatus,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    config: ChainHeadConfig,
}

impl<S: ChainHeadBlockSource> std::fmt::Debug for ChainHeadService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainHeadService")
            .field("status", &self.status.load())
            .field("best_tip", &self.current.load().best_tip())
            .finish_non_exhaustive()
    }
}

impl<S: ChainHeadBlockSource> ChainHeadService<S> {
    /// Anchors the graph, then starts the writer task that extends it.
    ///
    /// Anchoring is the old `initialize` with `resolve_anchor_block`: one block
    /// at the anchor height, which the writer task then extends one block at a
    /// time. Doing it before returning is what makes
    /// [`ChainHeadSubscriber::current`] total — there is no state in which a
    /// ChainHead exists with nothing to answer from.
    ///
    /// # Shutdown contract
    ///
    /// **Dropping the returned `Arc` does not stop the writer task.** The task
    /// holds its own `Arc<Self>`, so the service outlives every handle a caller
    /// keeps. Stop it by cancelling `cancel` or by calling
    /// [`shutdown`](Self::shutdown); a caller that does neither leaks the task
    /// for the life of the process.
    ///
    /// This is deliberate rather than an oversight. A writer that stopped when
    /// the last read handle went away would stop mid-request in any consumer
    /// that briefly holds no subscriber, and the task must outlive its handles
    /// to publish at all. The cost is that the caller owns the lifetime, so
    /// pass a token that is actually cancelled — see the cancellation section
    /// of this crate's `usage.md` for why it should be a *child* token.
    #[instrument(name = "ChainHeadService::spawn", skip_all, fields(max_depth = config.max_depth()))]
    pub async fn spawn(
        source: Arc<S>,
        config: ChainHeadConfig,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ChainHeadInitError> {
        let service = Self::anchored(source, config, cancel).await?;

        let worker = Arc::clone(&service);
        let handle = tokio::spawn(async move { worker.run().await });
        *service.task.lock().expect("chain head task mutex poisoned") = Some(handle);

        Ok(service)
    }

    /// An anchored service with **no writer task**, for tests that step it.
    ///
    /// Compiled out of production builds. Pair with
    /// [`advance_once`](Self::advance_once): with no writer running, a stepping
    /// test is the only thing advancing the graph, so what it observes is
    /// exactly what it caused.
    ///
    /// Shares [`anchored`](Self::anchored) with [`spawn`](Self::spawn), so the
    /// two construction paths cannot drift — they differ only in whether the
    /// task is started.
    #[cfg(any(test, feature = "testing"))]
    pub async fn spawn_without_writer(
        source: Arc<S>,
        config: ChainHeadConfig,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ChainHeadInitError> {
        Self::anchored(source, config, cancel).await
    }

    /// Advances the graph by one iteration and publishes the result.
    ///
    /// Compiled out of production builds. This is what the writer task does per
    /// tick; exposing it to tests lets them assert on a specific reorg shape
    /// without racing a timer.
    #[cfg(any(test, feature = "testing"))]
    pub async fn advance_once(&self) -> Result<(), ChainHeadAdvanceError> {
        self.tick().await
    }

    /// Everything [`spawn`](Self::spawn) does except start the task.
    async fn anchored(
        source: Arc<S>,
        config: ChainHeadConfig,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ChainHeadInitError> {
        let status = NamedAtomicStatus::new(COMPONENT, StatusType::Syncing);

        let snapshot = anchor_with_retry(&source, &config, &cancel).await?;
        info!(
            height = u32::from(snapshot.best_tip().height),
            hash = %snapshot.best_tip().hash,
            "ChainHead anchored"
        );

        let (updates, _) = watch::channel(ChainStateEpoch {
            generation: 0,
            best_tip: snapshot.best_tip(),
        });
        let (frozen, _) = broadcast::channel(FROZEN_CHANNEL_CAPACITY);

        let service = Arc::new(Self {
            source,
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
            updates,
            frozen,
            status,
            cancel,
            task: Mutex::new(None),
            config,
        });
        // Still `Syncing`: the anchor is the window's floor, not its tip, so a
        // reader served now would see a head up to `max_depth` below the
        // chain. `Ready` is published by the first successful advance, which
        // is the first moment the snapshot matches the validator's tip.

        Ok(service)
    }

    /// A read-only handle onto the published snapshot.
    ///
    /// The status cell is cloned, not read: the handle observes every later
    /// transition rather than the value that happened to hold here.
    pub fn subscriber(&self) -> ChainHeadSubscriber {
        ChainHeadSubscriber::new(
            Arc::clone(&self.current),
            self.updates.subscribe(),
            self.frozen.clone(),
            self.status.clone(),
        )
    }

    /// The runtime's current status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Stops the writer task.
    ///
    /// The cancellation token passed to [`spawn`](Self::spawn) also stops the
    /// task; this additionally publishes `Closing` and releases the handle, so
    /// shutdown is observable rather than merely effective.
    ///
    /// Synchronous, and does **not** wait for the task to wind down: it cancels
    /// and then aborts. It cannot wait, because it is called from `Drop`. The
    /// abort is safe rather than merely expedient — a snapshot is installed with
    /// one atomic store, so a task killed part-way through building a candidate
    /// leaves the last published snapshot whole. The status is stored before the
    /// abort so `Closing` is observable on every handle regardless of when the
    /// task dies.
    pub fn shutdown(&self) {
        self.status.store(StatusType::Closing);
        self.cancel.cancel();
        if let Some(handle) = self
            .task
            .lock()
            .expect("chain head task mutex poisoned")
            .take()
        {
            handle.abort();
        }
    }

    /// The writer task.
    ///
    /// The loop ChainIndex's sync worker ran for the non-finalised state, with
    /// the same backoff ladder and the same escalation to `CriticalError` after
    /// a run of failures.
    async fn run(self: Arc<Self>) {
        let mut wake = self.source.subscribe_to_blocks_received();
        let mut backoff = self.config.initial_backoff();
        let mut consecutive_failures = 0u32;

        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            let iteration = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                result = self.tick() => result,
            };

            match iteration {
                Ok(()) => {
                    consecutive_failures = 0;
                    backoff = self.config.initial_backoff();
                    // `Ready` is already published from inside `tick`, before
                    // the advanced snapshot becomes observable to readers.
                    if self.wait_for_work(&mut wake).await.is_break() {
                        break;
                    }
                }
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= self.config.max_consecutive_failures() {
                        warn!(
                            %error,
                            attempts = consecutive_failures,
                            "ChainHead giving up on the validator; last published snapshot is now stale",
                        );
                        self.status.apply(|s| next_status(s, TickOutcome::GaveUp));
                        break;
                    }
                    warn!(%error, attempts = consecutive_failures, "ChainHead failed to advance; retrying");
                    self.status.apply(|s| next_status(s, TickOutcome::Retrying));
                    if sleep_or_cancel(backoff, &self.cancel).await.is_break() {
                        break;
                    }
                    backoff = (backoff * 2).min(self.config.max_backoff());
                }
            }
        }

        debug!("ChainHead writer task stopped");
    }

    /// Waits for the poll interval, or for the source to say it has new blocks.
    ///
    /// The wake is a latency hint and nothing more: it carries no payload, and
    /// the next iteration re-reads the source regardless.
    async fn wait_for_work(
        &self,
        wake: &mut Option<watch::Receiver<()>>,
    ) -> std::ops::ControlFlow<()> {
        match wake {
            Some(rx) => tokio::select! {
                _ = self.cancel.cancelled() => std::ops::ControlFlow::Break(()),
                _ = tokio::time::sleep(self.config.poll_interval()) => std::ops::ControlFlow::Continue(()),
                changed = rx.changed() => {
                    if changed.is_err() {
                        // The source dropped its sender. Fall back to the
                        // interval for the rest of this runtime's life.
                        *wake = None;
                    }
                    std::ops::ControlFlow::Continue(())
                }
            },
            None => sleep_or_cancel(self.config.poll_interval(), &self.cancel).await,
        }
    }

    /// One iteration: read the tip, build the next graph, publish it.
    ///
    /// The old `sync`, less the publishing. `chain_height` is read once and the
    /// build is bounded by it, so a source advance mid-iteration — the
    /// validator producing blocks while this is still running — is deferred to
    /// the next iteration, which reads a fresh height and trims against the
    /// correct floor. Closes #1126.
    #[instrument(name = "ChainHeadService::tick", skip(self))]
    async fn tick(&self) -> Result<(), ChainHeadAdvanceError> {
        let tip = self.chain_tip().await?;
        let previous = self.current.load_full();

        // Nothing to do when the source's tip is the one we hold. A block hash
        // commits to its parent, so an identical tip means an identical chain
        // beneath it — there is no reorg hiding below a tip we agree on.
        //
        // This is what keeps a steady-state poll to a single question. Without
        // it every tick rebuilds the graph and re-reads the tip block to check
        // for a same-height reorg, which costs a round trip per poll for an
        // answer that cannot have changed.
        if tip == previous.best_tip() {
            self.mark_fresh();
            return Ok(());
        }

        let next = self.next_graph(&previous, tip.height).await?;
        self.mark_fresh();
        self.publish_snapshot(&previous, next);
        Ok(())
    }

    /// Publishes the [`TickOutcome::Advanced`] transition before the snapshot
    /// swap, so no reader can observe a fresh snapshot under a stale `Syncing`.
    fn mark_fresh(&self) {
        self.status.apply(|s| next_status(s, TickOutcome::Advanced));
    }

    /// Builds the graph as it should be at `chain_height`.
    ///
    /// Returns a value; it neither reads nor writes the published cell. The old
    /// code mutated the published snapshot's clone in place and swapped it from
    /// inside this path, which is what let a long catch-up publish
    /// intermediates.
    async fn next_graph(
        &self,
        previous: &MapBackedSnapshot,
        chain_height: Height,
    ) -> Result<MapBackedSnapshot, ChainHeadAdvanceError> {
        // Anchor floor: the chain head must never start more than the
        // configured depth below the chain tip. Previously this took the
        // greater of the finalised database's height and this floor; with the
        // database gone the floor is the whole rule, and it is the arm the old
        // code took whenever the database lagged (#1261).
        let anchor_height = height_below(chain_height, self.config.max_depth());

        let mut graph = if previous.best_tip().height < anchor_height {
            // The chain moved further than the window covers. Re-anchor rather
            // than walking the gap one block at a time.
            MapBackedSnapshot::from_initial_block(self.resolve_anchor_block(anchor_height).await?)
        } else {
            previous.clone()
        };

        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541
        while u32::from(graph.best_tip().height) < u32::from(chain_height) {
            let Some(block) = self
                .block_at_height(next_height(graph.best_tip().height))
                .await?
            else {
                break;
            };

            let parent_hash = block.header.prev_hash;
            if parent_hash == graph.best_tip().hash {
                // Normal chain progression
                let prev_block = graph
                    .blocks
                    .get(&graph.best_tip().hash)
                    .ok_or_else(|| {
                        ChainHeadAdvanceError::ReorgFailure(format!(
                            "graph is missing its own tip {:?}",
                            graph.best_tip()
                        ))
                    })?
                    .clone();
                let chainblock = self.block_to_chainblock(&prev_block, &block).await?;
                info!(
                    height = u32::from(chainblock.height()),
                    hash = %chainblock.hash(),
                    "Syncing block"
                );
                graph.add_block_new_chaintip(chainblock);
            } else {
                // There's been a reorg. The fresh block is the new chaintip; we
                // work backwards from it and update heights_to_hashes with it
                // and all its parents.
                self.handle_reorg(&mut graph, &block, 0).await?;
            }
        }

        self.check_for_nonhigher_reorgs(&mut graph, None).await?;

        // Trim to a fixed window below the tip. This was the greater of the
        // finalised database's height and this tip-relative cap; the cap is now
        // the whole rule, and it is what bounded memory before whenever the
        // database under-reported or was pinned at zero in ephemeral mode.
        graph.remove_finalized_blocks(height_below(
            graph.best_tip().height,
            self.max_retained_depth(),
        ));

        // Best chain is the most-work branch retained, which a reorg may have
        // left as something other than the block we just extended to.
        //
        // Strictly more work, not merely equal: two blocks at one height with
        // the same difficulty carry the same accumulated work, and picking
        // between them by which the map happened to yield last would let a tie
        // flip the tip away from the block the validator just told us is
        // canonical. On a tie the validator's answer — which the walk above has
        // already applied — wins.
        let tip_work = graph
            .blocks
            .get(&graph.best_tip().hash)
            .map(|block| block.work)
            .ok_or_else(|| {
                ChainHeadAdvanceError::ReorgFailure(format!(
                    "graph is missing its own tip {:?}",
                    graph.best_tip()
                ))
            })?;
        let heaviest = graph
            .blocks
            .values()
            .max_by_key(|block| block.work)
            .cloned()
            .expect("a graph always retains at least its anchor");
        if heaviest.work > tip_work {
            self.handle_reorg(&mut graph, &heaviest, 0).await?;
        }

        Ok(graph)
    }

    /// Handle a blockchain reorg by finding the common ancestor.
    async fn handle_reorg(
        &self,
        graph: &mut MapBackedSnapshot,
        block: &impl Block,
        recursion_count: u8,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        // We should never recurse back more than the retained window, assuming
        // a complete reorg of the whole graph.
        if u32::from(recursion_count) > self.max_retained_depth() {
            return Err(ChainHeadAdvanceError::ReorgFailure(
                "reorg handling recursed beyond reason".to_string(),
            ));
        }
        let prev_block = match graph.blocks.get(&block.parent_hash()).cloned() {
            Some(prev_block) => {
                if graph.is_on_best_chain(prev_block.reference) {
                    prev_block
                } else {
                    Box::pin(self.handle_reorg(graph, &prev_block, recursion_count + 1)).await?
                }
            }
            None => {
                let prev_block = self.block_at_hash(block.parent_hash()).await?.ok_or(
                    ChainHeadAdvanceError::InconsistentSource(format!(
                        "validator is missing block {}, the parent of one it served",
                        block.parent_hash()
                    )),
                )?;
                Box::pin(self.handle_reorg(graph, &prev_block, recursion_count + 1)).await?
            }
        };
        let chainblock = block.to_chain_head_block(&prev_block, self).await?;
        graph.add_block_new_chaintip(chainblock.clone());
        Ok(chainblock)
    }

    /// Catches a reorg that did not raise the tip.
    ///
    /// The extension loop only notices a reorg when it finds a *higher* block
    /// whose parent it does not hold. A branch swap at the same height, or a
    /// rollback, produces no such block — this is what sees those.
    async fn check_for_nonhigher_reorgs(
        &self,
        graph: &mut MapBackedSnapshot,
        // Callers should provide None. Used for self-recursion case only.
        height_to_recurse_to: Option<Height>,
    ) -> Result<(), ChainHeadAdvanceError> {
        if height_to_recurse_to.is_some_and(|height| {
            u32::from(height) + self.max_retained_depth() < u32::from(graph.best_tip().height)
        }) {
            return Err(ChainHeadAdvanceError::ReorgFailure(
                "reorg detection recursed beyond reason".to_string(),
            ));
        }
        let target_height = height_to_recurse_to.unwrap_or(graph.best_tip().height);
        match self.block_at_height(target_height).await? {
            Some(block) => {
                if block.header.hash != graph.best_tip().hash {
                    self.handle_reorg(graph, &block, 0).await?;
                }
                Ok(())
            }
            None => {
                // The source cannot serve this height. Walk down until it can,
                // bounded by the retained window above.
                if u32::from(target_height) == 0 {
                    return Ok(());
                }
                Box::pin(
                    self.check_for_nonhigher_reorgs(graph, Some(height_below(target_height, 1))),
                )
                .await
            }
        }
    }

    /// Installs a snapshot and tells everyone what changed.
    ///
    /// A plain `store`: the writer task is the only writer, so there is nothing
    /// to lose a compare-and-swap race against. One store per iteration is what
    /// makes a published snapshot always whole.
    fn publish_snapshot(&self, previous: &MapBackedSnapshot, mut next: MapBackedSnapshot) {
        let (stale_tip, new_tip) = (previous.best_tip(), next.best_tip());
        let tip_changed = new_tip != stale_tip;

        // Blocks that crossed the consensus seam during this iteration are now
        // beyond the reach of any reorg, so they can be handed to a store. The
        // seam sits at the configured depth; the retention floor is lower, so
        // a frozen block is still retained for a while after it is emitted.
        let frozen: Vec<ChainHeadBlock> = if self.frozen.receiver_count() > 0 {
            let was_frozen_below = height_below(stale_tip.height, self.config.max_depth());
            let now_frozen_below = height_below(new_tip.height, self.config.max_depth());
            next.best_chain()
                .filter(|block| {
                    block.height() > was_frozen_below && block.height() <= now_frozen_below
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        // Stamped *before* the store, so a reader that captures the view and
        // asks for its epoch is told the epoch this publication carries rather
        // than whatever has been published since. The rule for which generation
        // that is belongs to the snapshot; this only supplies the two facts it
        // cannot know — what came before, and how far the epoch has ever got.
        //
        // The highest published generation is read into a local first because
        // `borrow()` holds a read guard for the whole enclosing statement and
        // the `send_replace` below wants the write lock, so inlining the read
        // deadlocks the channel against itself.
        let highest_published = self.updates.borrow().generation;
        next.stamp_generation(previous, highest_published);
        let generation = next.epoch().generation;

        self.current.store(Arc::new(next));

        if tip_changed {
            log_tip_change(stale_tip, new_tip);
            #[cfg(feature = "prometheus")]
            record_reorg(stale_tip, new_tip);

            self.updates.send_replace(ChainStateEpoch {
                generation,
                best_tip: new_tip,
            });
        }

        for block in frozen {
            // A full channel drops the oldest; the consumer sees `Lagged` and
            // rebuilds the gap from its own source, which it can always do.
            let _ = self.frozen.send(block);
        }
    }

    /// How far below the tip blocks are retained.
    fn max_retained_depth(&self) -> u32 {
        self.config.max_depth().saturating_add(RETENTION_MARGIN)
    }

    async fn block_to_chainblock(
        &self,
        prev_block: &ChainHeadBlock,
        block: &zaino_primitives::types::Block,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let tree_roots = self.tree_roots(block.header.hash).await?;
        chain_head_block(block.clone(), &tree_roots, Some(prev_block.work))
    }

    /// Get commitment tree roots from the blockchain source.
    async fn tree_roots(&self, hash: BlockHash) -> Result<TreeRoots, ChainHeadAdvanceError> {
        self.source
            .get_commitment_tree_roots(hash)
            .await
            .map_err(|error| {
                ChainHeadAdvanceError::InconsistentSource(format!(
                    "tree roots for block {hash}: {error}"
                ))
            })
    }

    /// Resolve the chain head's anchor (root) block at `anchor_height`.
    ///
    /// The finalised-reader arm is gone with the finalised state; what remains
    /// is the fallback the old code used whenever the reader could not serve
    /// the height, which was every time the database lagged.
    ///
    /// The anchor sits below the reorg-possible range, so its accumulated work
    /// is the base of this window's own accumulation rather than an absolute
    /// value — see `ChainHeadWork`.
    async fn resolve_anchor_block(
        &self,
        anchor_height: Height,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let block = self.block_at_height(anchor_height).await?.ok_or_else(|| {
            ChainHeadAdvanceError::InconsistentSource(format!(
                "anchor block {anchor_height} unavailable from validator"
            ))
        })?;

        let tree_roots = self.tree_roots(block.header.hash).await?;
        chain_head_block(block, &tree_roots, None)
    }

    /// One coherent height/hash pair from the source.
    async fn chain_tip(&self) -> Result<BlockRef, ChainHeadAdvanceError> {
        let (hash, height) = self
            .source
            .get_chain_tip()
            .await
            .map_err(|error| ChainHeadAdvanceError::SourceUnavailable(error.to_string()))?;
        Ok(BlockRef { hash, height })
    }

    /// A best-chain block by height. `None` when the source has no such block.
    async fn block_at_height(
        &self,
        height: Height,
    ) -> Result<Option<zaino_primitives::types::Block>, ChainHeadAdvanceError> {
        match self.source.get_block(height).await {
            Ok(block) => Ok(Some(block)),
            // Absent, not failed: the extension loop reads past the tip by
            // design, which is how it learns where the tip is.
            Err(zaino_source::QueryError::Domain(_)) => Ok(None),
            Err(error) => Err(ChainHeadAdvanceError::SourceUnavailable(error.to_string())),
        }
    }

    /// A block by hash, side-chain blocks included.
    async fn block_at_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<zaino_primitives::types::Block>, ChainHeadAdvanceError> {
        match self.source.get_block_by_hash(hash).await {
            Ok(block) => Ok(Some(block)),
            Err(zaino_source::QueryError::Domain(_)) => Ok(None),
            Err(error) => Err(ChainHeadAdvanceError::SourceUnavailable(error.to_string())),
        }
    }
}

impl<S: ChainHeadBlockSource> Status for ChainHeadService<S> {
    fn status(&self) -> StatusType {
        self.status.load()
    }
}

impl<S: ChainHeadBlockSource> Drop for ChainHeadService<S> {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self
            .task
            .lock()
            .expect("chain head task mutex poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

/// What one writer iteration concluded about the published snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TickOutcome {
    /// The snapshot now matches the validator tip read this iteration.
    Advanced,
    /// The advance failed and the backoff ladder will retry it.
    Retrying,
    /// The advance failed `max_consecutive_failures` times and the writer is exiting.
    GaveUp,
}

/// The chain head's status transition rule, pure and total so its invariants
/// are stated once here instead of re-derived at every store site.
fn next_status(current: StatusType, outcome: TickOutcome) -> StatusType {
    match (current, outcome) {
        // Closing absorbs every outcome: a shutdown that races the final
        // iteration must stay observable on every handle.
        (StatusType::Closing, _) => StatusType::Closing,
        (_, TickOutcome::Advanced) => StatusType::Ready,
        (_, TickOutcome::Retrying) => StatusType::RecoverableError,
        (_, TickOutcome::GaveUp) => StatusType::CriticalError,
    }
}

/// Builds a [`ChainHeadBlock`], accumulating work onto its parent's.
///
/// The old `create_indexed_block_with_optional_roots`, less the parts only a
/// persisted block needed. `parent_work` is `None` only for the anchor, whose
/// accumulation starts at its own work — see `ChainHeadWork` for why that is
/// anchor-relative rather than absolute.
fn chain_head_block(
    block: zaino_primitives::types::Block,
    tree_roots: &TreeRoots,
    parent_work: Option<ChainHeadWork>,
) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
    let block_work = zaino_consensus::work_from_bits(block.header.bits).map_err(|error| {
        ChainHeadAdvanceError::InconsistentSource(format!(
            "block {} has invalid difficulty: {error}",
            block.header.hash
        ))
    })?;

    let work = match parent_work {
        Some(parent) => parent.checked_add(block_work).ok_or_else(|| {
            ChainHeadAdvanceError::ReorgFailure(format!(
                "accumulated work overflowed at block {}",
                block.header.hash
            ))
        })?,
        None => ChainHeadWork::anchored_at(block_work),
    };

    Ok(ChainHeadBlock {
        reference: BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        work,
        block,
        tree_roots: tree_roots.clone(),
    })
}

/// Anchors the graph, retrying transient source failures.
async fn anchor_with_retry<S: ChainHeadBlockSource>(
    source: &Arc<S>,
    config: &ChainHeadConfig,
    cancel: &CancellationToken,
) -> Result<MapBackedSnapshot, ChainHeadInitError> {
    let mut backoff = config.initial_backoff();
    let mut failures = 0u32;

    loop {
        if cancel.is_cancelled() {
            return Err(ChainHeadInitError::Cancelled);
        }

        match anchor(source, config).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                failures += 1;
                if failures >= config.max_consecutive_failures() {
                    return Err(ChainHeadInitError::SourceUnavailable {
                        attempts: failures,
                        source: error,
                    });
                }
                warn!(%error, attempt = failures, "ChainHead anchoring failed; retrying");
                if sleep_or_cancel(backoff, cancel).await.is_break() {
                    return Err(ChainHeadInitError::Cancelled);
                }
                backoff = (backoff * 2).min(config.max_backoff());
            }
        }
    }
}

/// One attempt at anchoring: the block at `tip - depth`, alone.
///
/// The writer task extends from here one block at a time, exactly as before.
async fn anchor<S: ChainHeadBlockSource>(
    source: &Arc<S>,
    config: &ChainHeadConfig,
) -> Result<MapBackedSnapshot, ChainHeadAdvanceError> {
    let (_, tip_height) = source
        .get_chain_tip()
        .await
        .map_err(|error| ChainHeadAdvanceError::SourceUnavailable(error.to_string()))?;

    let anchor_height = height_below(tip_height, config.max_depth());

    let block = source
        .get_block(anchor_height)
        .await
        .map_err(|error| ChainHeadAdvanceError::SourceUnavailable(error.to_string()))?;
    let tree_roots = source
        .get_commitment_tree_roots(block.header.hash)
        .await
        .map_err(|error| ChainHeadAdvanceError::InconsistentSource(error.to_string()))?;

    Ok(MapBackedSnapshot::from_initial_block(chain_head_block(
        block,
        &tree_roots,
        None,
    )?))
}

/// Sleeps, unless cancelled first.
async fn sleep_or_cancel(
    duration: Duration,
    cancel: &CancellationToken,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        _ = cancel.cancelled() => std::ops::ControlFlow::Break(()),
        _ = tokio::time::sleep(duration) => std::ops::ControlFlow::Continue(()),
    }
}

/// `height - delta`, saturating at genesis.
fn height_below(height: Height, delta: u32) -> Height {
    height.saturating_sub(delta)
}

/// The height one above `height`, saturating at the protocol maximum.
fn next_height(height: Height) -> Height {
    height.checked_add(1).unwrap_or(height)
}

fn log_tip_change(old: BlockRef, new: BlockRef) {
    let (old_height, new_height) = (u32::from(old.height), u32::from(new.height));
    if new_height > old_height {
        info!(old_height, new_height, new_hash = %new.hash, "Chain head tip advanced");
    } else if new_height == old_height {
        info!(height = new_height, old_hash = %old.hash, new_hash = %new.hash, "Chain head tip reorg");
    } else {
        info!(old_height, new_height, new_hash = %new.hash, "Chain head tip rollback");
    }
}

/// Reports a tip change that was not a simple advance.
///
/// A tip moving forward is the chain working; a tip replaced at the same height
/// or moving backwards is a reorganisation, and the depth is how far the chain
/// was rewritten. Only the latter is counted, so the rate reflects reorgs
/// rather than block production.
#[cfg(feature = "prometheus")]
fn record_reorg(old: BlockRef, new: BlockRef) {
    use crate::metric_names::{CHAIN_HEAD_REORG_DEPTH, CHAIN_HEAD_REORG_TOTAL};

    let (old_height, new_height) = (u32::from(old.height), u32::from(new.height));
    if new_height > old_height {
        return;
    }

    metrics::counter!(CHAIN_HEAD_REORG_TOTAL).increment(1);
    metrics::histogram!(CHAIN_HEAD_REORG_DEPTH).record(f64::from(old_height - new_height));
}

/// Lets the reorg walk take either a block already in the graph or one just
/// fetched from the source, as the original's private `Block` trait did.
///
/// The original compared serialized-order hash bytes because its two block
/// types disagreed about byte order. Both sides now carry the domain's
/// [`BlockHash`], so the comparison is direct.
trait Block {
    fn parent_hash(&self) -> BlockHash;
    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError>;
}

impl Block for ChainHeadBlock {
    fn parent_hash(&self) -> BlockHash {
        self.parent_hash
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        _prev_block: &ChainHeadBlock,
        _service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        Ok(self.clone())
    }
}

impl Block for zaino_primitives::types::Block {
    fn parent_hash(&self) -> BlockHash {
        self.header.prev_hash
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        service.block_to_chainblock(prev_block, self).await
    }
}

#[cfg(test)]
mod next_status_rule {
    use super::{next_status, StatusType, TickOutcome};

    const OUTCOMES: [TickOutcome; 3] = [
        TickOutcome::Advanced,
        TickOutcome::Retrying,
        TickOutcome::GaveUp,
    ];

    /// No outcome may overwrite `Closing`, so a shutdown stays observable.
    #[test]
    fn closing_absorbs_every_outcome() {
        for outcome in OUTCOMES {
            assert_eq!(
                next_status(StatusType::Closing, outcome),
                StatusType::Closing,
                "{outcome:?} must not overwrite Closing"
            );
        }
    }

    /// Every non-`Closing` state takes the status its outcome names.
    #[test]
    fn every_live_state_takes_the_outcome_status() {
        let live_states = [
            StatusType::Spawning,
            StatusType::Syncing,
            StatusType::Ready,
            StatusType::Busy,
            StatusType::RecoverableError,
            StatusType::CriticalError,
            StatusType::Offline,
        ];
        for current in live_states {
            assert_eq!(
                next_status(current, TickOutcome::Advanced),
                StatusType::Ready
            );
            assert_eq!(
                next_status(current, TickOutcome::Retrying),
                StatusType::RecoverableError
            );
            assert_eq!(
                next_status(current, TickOutcome::GaveUp),
                StatusType::CriticalError
            );
        }
    }
}
