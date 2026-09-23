//! The ChainHead runtime.
//!
//! The graph advancement, reorg handling and retention below are the
//! non-finalised state's, moved here and changed only where the new boundaries
//! force it:
//!
//! - the finalised state is gone. The old `sync` took an `Arc<FinalisedState>`
//!   and read `db_height()` for both its window floor and its trim floor; both
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
    fmt,
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
use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, StatusSource};
use zaino_primitives::types::{BlockHash, BlockRef, ChainStateEpoch, Height, TreeRoots};
use zaino_status::{NamedAtomicStatus, StatusType};

use crate::{
    error::{ChainHeadAdvanceError, ChainHeadInitError},
    snapshot::MapBackedSnapshot,
    subscriber::ChainHeadSubscriber,
};

/// The name this component reports status under.
const COMPONENT: &str = "ChainHead";

/// Retention margin below the configured non-finalized depth.
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
    /// Anchoring is the old `initialize` with `resolve_floor_block`: one block
    /// at the window floor, which the writer task then extends one block at a
    /// time. Doing it before returning is what makes
    /// `ChainHeadSubscriber::current` total — there is no state in which a
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
    /// Shares `anchored` with [`spawn`](Self::spawn), so the
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

        let snapshot = floor_with_retry(&source, &config, &cancel).await?;
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
        // Still `Syncing`: the graph starts at the window floor, not at the
        // tip, so a reader served now would see a head up to `max_depth` below
        // the chain. `Ready` is published by the first successful advance,
        // which is the first moment the snapshot matches the validator's tip.

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
    pub fn status(&self) -> ComponentStatus {
        component_status(&self.status)
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
        let floor_height = height_below(chain_height, self.config.max_depth());

        let mut graph = if previous.best_tip().height < floor_height {
            // The chain moved further than the window covers. Re-anchor rather
            // than walking the gap one block at a time.
            MapBackedSnapshot::from_initial_block(self.resolve_floor_block(floor_height).await?)
        } else {
            previous.clone()
        };

        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541
        while u32::from(graph.best_tip().height) < u32::from(chain_height) {
            let next = next_height(graph.best_tip().height);
            // Both reads address the best-chain block at `next`, so they are
            // independent and run concurrently; the hash check below closes
            // the reorg race the concurrency opens.
            let (block, roots) = tokio::join!(
                self.block_at_height(next),
                self.source.get_commitment_tree_roots_by_height(next),
            );
            let Some(block) = block? else {
                // Past the tip; the roots result is dropped unexamined, since
                // its `HeightNotFound` there is expected, not a failure.
                break;
            };

            let parent_hash = block.header.prev_hash;
            if parent_hash == graph.best_tip().hash {
                // Normal chain progression
                let prev_block = graph
                    .tip_block()
                    .ok_or_else(|| {
                        ChainHeadAdvanceError::ReorgFailure(format!(
                            "graph is missing its own tip {:?}",
                            graph.best_tip()
                        ))
                    })?
                    .clone();
                let roots = match roots {
                    Ok((roots_hash, roots)) if roots_hash == block.header.hash => roots,
                    // A reorg swapped the best-chain block between the two
                    // reads, or the height-addressed read failed; the hash we
                    // hold is authoritative, so refetch by it.
                    _ => self.tree_roots(block.header.hash).await?,
                };
                let work = accumulated_work(&prev_block, &block)?;
                let chainblock = chain_head_block(block.clone(), &roots, work);
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
                self.handle_reorg(&mut graph, &block).await?;
            }
        }

        self.check_for_nonhigher_reorgs(&mut graph).await?;

        // Trim to a fixed window below the tip.
        //
        // Best chain is the most-work branch retained, which a reorg may have
        // left as something other than the block we just extended to.
        //
        // Strictly more work, not merely equal: two blocks at one height with
        // the same difficulty carry the same accumulated work, and picking
        // between them by which the map happened to yield last would let a tie
        // flip the tip away from the block the validator just told us is
        // canonical. On a tie the validator's answer — which the walk above has
        // already applied — wins.
        let tip_work = graph.tip_block().map(|block| block.work).ok_or_else(|| {
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
            .expect("a graph always retains at least its floor block");
        if heaviest.work > tip_work {
            self.handle_reorg(&mut graph, &heaviest).await?;
        }

        Ok(graph)
    }

    /// Handle a blockchain reorg by finding the common ancestor.
    ///
    /// Walks down from `block` to the first ancestor on the best chain,
    /// collecting the branch on the way, then lays that branch back down from
    /// the fork point up. The descent holds its pending blocks in a `Vec`
    /// rather than in call frames, so its depth costs heap and not stack.
    async fn handle_reorg(
        &self,
        graph: &mut MapBackedSnapshot,
        block: &impl Block,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let mut branch: Vec<BranchBlock> = Vec::new();
        let mut parent_hash = block.parent_hash();
        let mut descended: u32 = 0;

        // Down to the fork point. A branch longer than the retained window
        // cannot rejoin the best chain within it, so the walk stops rather than
        // asking the validator for ancestors all the way to genesis.
        let fork_point = loop {
            if descended > self.max_retained_depth() {
                return Err(ChainHeadAdvanceError::ReorgFailure(
                    "reorg handling walked beyond the retained window".to_string(),
                ));
            }
            descended = descended.saturating_add(1);

            match graph.blocks.get(&parent_hash).cloned() {
                Some(prev_block) if graph.is_on_best_chain(prev_block.reference) => {
                    break prev_block
                }
                Some(prev_block) => {
                    parent_hash = prev_block.parent_hash;
                    branch.push(BranchBlock::Retained(Box::new(prev_block)));
                }
                None => {
                    let fetched = self.block_at_hash(parent_hash).await?.ok_or(
                        ChainHeadAdvanceError::InconsistentSource(format!(
                            "validator is missing block {parent_hash}, the parent of one it served"
                        )),
                    )?;
                    parent_hash = fetched.header.prev_hash;
                    branch.push(BranchBlock::Fetched(Box::new(fetched)));
                }
            }
        };

        // Back up from the fork point, oldest first, ending on the block that
        // started the walk. Each block's work accumulates from the one below,
        // so they go on in order.
        let mut prev_block = fork_point;
        for pending in branch.into_iter().rev() {
            prev_block = pending.to_chain_head_block(&prev_block, self).await?;
            graph.add_block_new_chaintip(prev_block.clone());
        }
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
    ) -> Result<(), ChainHeadAdvanceError> {
        let tip = graph.best_tip();
        let mut target_height = tip.height;

        loop {
            if let Some(block) = self.block_at_height(target_height).await? {
                if block.header.hash != tip.hash {
                    self.handle_reorg(graph, &block).await?;
                }
                return Ok(());
            }

            // The source cannot serve this height. Step down until it can,
            // bounded by the retained window: below that there is nothing left
            // in the graph for a reorg to rejoin.
            if u32::from(target_height) == 0 {
                return Ok(());
            }
            target_height = height_below(target_height, 1);
            if u32::from(target_height) + self.max_retained_depth() < u32::from(tip.height) {
                return Err(ChainHeadAdvanceError::ReorgFailure(
                    "reorg detection stepped below the retained window".to_string(),
                ));
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

        // Read from the snapshot being installed: the fork point is the first
        // ancestor of the old tip that the *new* chain still calls canonical.
        // Before the trim below: the lookup needs the old tip still in the graph
        let fork = tip_changed
            .then(|| next.find_fork_point(&stale_tip.hash))
            .flatten();
        // Read beside the fork point, so both describe the same pre-trim graph
        let retained_floor = next.lowest_retained_height();

        // Trim only now. The handoff above reads the blocks it emits out of the
        // graph, so a block has to still be there to be handed off: trimming
        // first bounds one iteration's handoff by the gap between the seam and
        // the retention floor, and silently settles the rest without emitting
        // them. The order is the guarantee, not the size of that gap.
        next.remove_finalized_blocks(height_below(
            next.best_tip().height,
            self.max_retained_depth(),
        ));

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
            record_reorg(stale_tip, fork, retained_floor);

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
        let work = accumulated_work(prev_block, block)?;
        Ok(chain_head_block(block.clone(), &tree_roots, work))
    }

    /// Get commitment tree roots from the blockchain source.
    async fn tree_roots(&self, hash: BlockHash) -> Result<TreeRoots, ChainHeadAdvanceError> {
        self.source
            .get_commitment_tree_roots(hash)
            .await
            .map_err(|error| advance_error(error, &format!("tree roots for block {hash}")))
    }

    /// Resolve the window floor — the graph's root block — at `floor_height`.
    ///
    /// The finalised-reader arm is gone with the finalised state; what remains
    /// is the fallback the old code used whenever the reader could not serve
    /// the height, which was every time the database lagged.
    ///
    /// The floor sits below the reorg-possible range, so its work is the base
    /// of this window's own accumulation rather than an absolute value — see
    /// `ChainHeadWork`.
    async fn resolve_floor_block(
        &self,
        floor_height: Height,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let block = self.block_at_height(floor_height).await?.ok_or_else(|| {
            ChainHeadAdvanceError::InconsistentSource(format!(
                "floor block {floor_height} unavailable from validator"
            ))
        })?;

        let tree_roots = self.tree_roots(block.header.hash).await?;
        let work = ChainHeadWork::anchored_at(block_work(&block));
        Ok(chain_head_block(block, &tree_roots, work))
    }

    /// One coherent height/hash pair from the source.
    async fn chain_tip(&self) -> Result<BlockRef, ChainHeadAdvanceError> {
        let (hash, height) = self
            .source
            .get_chain_tip()
            .await
            .map_err(|error| advance_error(error, "chain tip"))?;
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
            // design, which is how it learns where the tip is. Matched by name
            // rather than a wildcard so a future second domain variant breaks
            // the build here — the one site that must reclassify it — instead
            // of being silently read as end-of-chain.
            Err(zaino_source::QueryError::Domain(zaino_source::GetBlockError::HeightNotFound(
                missing,
            ))) => {
                debug!(height = %missing, "block_at_height: source reports no block; treating as absent");
                Ok(None)
            }
            // Transport failure carries its cause through unchanged.
            Err(zaino_source::QueryError::Fetch(fetch)) => {
                Err(ChainHeadAdvanceError::SourceUnavailable(fetch))
            }
        }
    }

    /// A block by hash, side-chain blocks included.
    async fn block_at_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<zaino_primitives::types::Block>, ChainHeadAdvanceError> {
        match self.source.get_block_by_hash(hash).await {
            Ok(block) => Ok(Some(block)),
            // Absent, not failed. Matched by name, not a wildcard, so a future
            // second domain variant is caught by the compiler here rather than
            // silently reclassified as absent.
            Err(zaino_source::QueryError::Domain(zaino_source::GetBlockByHashError::NotFound(
                missing,
            ))) => {
                debug!(hash = %missing, "block_at_hash: source reports no block; treating as absent");
                Ok(None)
            }
            // Transport failure carries its cause through unchanged.
            Err(zaino_source::QueryError::Fetch(fetch)) => {
                Err(ChainHeadAdvanceError::SourceUnavailable(fetch))
            }
        }
    }
}

impl<S: ChainHeadBlockSource> StatusSource for ChainHeadService<S> {
    fn status(&self) -> ComponentStatus {
        component_status(&self.status)
    }
}

/// A status cell's fused value, as the two axes a component reports.
///
/// Transitional, and the only place the two vocabularies meet here. The
/// runtime tracks the fused [`StatusType`] throughout — `next_status` still
/// folds a tick outcome into one value — and this re-splits it at the reporting
/// boundary. It goes when the runtime holds a phase and a condition separately.
pub(crate) fn component_status(status: &NamedAtomicStatus) -> ComponentStatus {
    let (lifecycle, health) = match status.load() {
        StatusType::Spawning => (Lifecycle::Spawning, Health::Healthy),
        StatusType::Syncing => (Lifecycle::Syncing, Health::Healthy),
        StatusType::Ready => (Lifecycle::Ready, Health::Healthy),
        StatusType::Closing => (Lifecycle::Closing, Health::Healthy),
        StatusType::Offline => (Lifecycle::Offline, Health::Offline),
        StatusType::Busy | StatusType::RecoverableError => {
            (Lifecycle::Syncing, Health::Recoverable)
        }
        StatusType::CriticalError => (Lifecycle::Offline, Health::Critical),
    };

    ComponentStatus::new(ComponentName(status.name()), lifecycle, health)
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

/// Classifies a source [`QueryError`](zaino_source::QueryError) into a
/// [`ChainHeadAdvanceError`], the single home of the transport-vs-domain split.
///
/// A transport failure is threaded through unchanged as the `#[source]` of
/// [`SourceUnavailable`](ChainHeadAdvanceError::SourceUnavailable), so
/// `Error::source()` yields the underlying [`FetchError`](zaino_source::FetchError)
/// and its machine-readable failure mode. A domain rejection wraps no external
/// error, so it stays message-only under
/// [`InconsistentSource`](ChainHeadAdvanceError::InconsistentSource), tagged
/// with `context` to name the query that was refused.
fn advance_error<E: fmt::Debug + fmt::Display>(
    error: zaino_source::QueryError<E>,
    context: &str,
) -> ChainHeadAdvanceError {
    match error {
        zaino_source::QueryError::Fetch(fetch) => ChainHeadAdvanceError::SourceUnavailable(fetch),
        zaino_source::QueryError::Domain(domain) => {
            ChainHeadAdvanceError::InconsistentSource(format!("{context}: {domain}"))
        }
    }
}

/// This block's own work, from its difficulty.
fn block_work(block: &zaino_primitives::types::Block) -> u128 {
    std::num::NonZeroU128::from(block.header.bits.to_work()).get()
}

/// `block`'s work, accumulated onto its parent's.
fn accumulated_work(
    prev_block: &ChainHeadBlock,
    block: &zaino_primitives::types::Block,
) -> Result<ChainHeadWork, ChainHeadAdvanceError> {
    prev_block
        .work
        .checked_add(block_work(block))
        .ok_or_else(|| {
            ChainHeadAdvanceError::ReorgFailure(format!(
                "accumulated work overflowed at block {}",
                block.header.hash
            ))
        })
}

/// Builds a [`ChainHeadBlock`] carrying `work`.
fn chain_head_block(
    block: zaino_primitives::types::Block,
    tree_roots: &TreeRoots,
    work: ChainHeadWork,
) -> ChainHeadBlock {
    ChainHeadBlock {
        reference: BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        work,
        block,
        tree_roots: tree_roots.clone(),
    }
}

/// Anchors the graph, retrying transient source failures.
async fn floor_with_retry<S: ChainHeadBlockSource>(
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

        match window_floor(source, config).await {
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

/// One attempt at anchoring: the window floor, the block at `tip - depth`,
/// alone.
///
/// The writer task extends from here one block at a time, exactly as before.
async fn window_floor<S: ChainHeadBlockSource>(
    source: &Arc<S>,
    config: &ChainHeadConfig,
) -> Result<MapBackedSnapshot, ChainHeadAdvanceError> {
    let (_, tip_height) = source
        .get_chain_tip()
        .await
        .map_err(|error| advance_error(error, "chain tip"))?;

    let floor_height = height_below(tip_height, config.max_depth());

    let block = source
        .get_block(floor_height)
        .await
        .map_err(|error| advance_error(error, &format!("floor block {floor_height}")))?;
    let tree_roots = source
        .get_commitment_tree_roots(block.header.hash)
        .await
        .map_err(|error| {
            advance_error(
                error,
                &format!("tree roots for block {}", block.header.hash),
            )
        })?;

    let work = ChainHeadWork::anchored_at(block_work(&block));
    Ok(MapBackedSnapshot::from_initial_block(chain_head_block(
        block,
        &tree_roots,
        work,
    )))
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

/// How a tip change relates to the tip it replaced
///
/// - `Reorg` depth = blocks rewritten; `None` = fork point below the retained window
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TipChange {
    Advance,
    Reorg(Option<u32>),
}

/// Classifies a tip change from the fork point the new snapshot reports.
///
/// - Reorg = the old tip stopped being canonical, whatever the heights. A branch won
///   by a *longer* chain is the ordinary case, and comparing heights misses it
/// - Depth = blocks rewritten (old tip - fork), not height lost: an equal-height swap
///   rewrites one block, and a longer branch rewrites what it replaced
/// - No fork point and the old tip below `retained_floor` = the chain advanced past
///   the window (or the graph re-anchored), so the old tip was dropped, not replaced;
///   a reorg that deep is beyond the consensus seam the window is sized to
pub(crate) fn classify_tip_change(
    old: BlockRef,
    fork: Option<BlockRef>,
    retained_floor: Height,
) -> TipChange {
    match fork {
        // The old tip is still canonical, so the new one descends from it
        Some(fork) if fork == old => TipChange::Advance,
        Some(fork) => TipChange::Reorg(Some(
            u32::from(old.height).saturating_sub(u32::from(fork.height)),
        )),
        None if old.height < retained_floor => TipChange::Advance,
        None => TipChange::Reorg(None),
    }
}

/// Reports a tip change that rewrote part of the chain.
fn record_reorg(old: BlockRef, fork: Option<BlockRef>, retained_floor: Height) {
    use crate::metric_names::{CHAIN_HEAD_REORG_DEPTH, CHAIN_HEAD_REORG_TOTAL};

    let depth = match classify_tip_change(old, fork, retained_floor) {
        TipChange::Advance => return,
        TipChange::Reorg(depth) => depth,
    };

    metrics::counter!(CHAIN_HEAD_REORG_TOTAL).increment(1);
    match depth {
        Some(depth) => metrics::histogram!(CHAIN_HEAD_REORG_DEPTH).record(f64::from(depth)),
        // Counted but not measured, so `reorg_total` can exceed the histogram's count
        None => warn!(
            old_tip = %old.hash,
            "reorg forked below the retained window; depth unknown"
        ),
    }
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

/// One block on a branch the reorg walk has yet to lay down.
///
/// The walk meets two kinds on its way to the fork point: blocks the graph
/// still holds on a branch that lost, and blocks it has to ask the validator
/// for. Both go on the same pending list, so the list has one type.
/// Both variants are boxed: a whole block runs to well over a kilobyte, and the
/// list holds one entry per block on the branch.
enum BranchBlock {
    /// Retained by the graph, on a branch that is not the best chain.
    Retained(Box<ChainHeadBlock>),
    /// Fetched from the validator, because the graph had dropped it.
    Fetched(Box<zaino_primitives::types::Block>),
}

impl Block for BranchBlock {
    fn parent_hash(&self) -> BlockHash {
        match self {
            Self::Retained(block) => block.parent_hash(),
            Self::Fetched(block) => block.parent_hash(),
        }
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        match self {
            Self::Retained(block) => block.to_chain_head_block(prev_block, service).await,
            Self::Fetched(block) => block.to_chain_head_block(prev_block, service).await,
        }
    }
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

#[cfg(test)]
mod reorg_tests {
    use super::*;

    fn at(height: u32, hash: u8) -> BlockRef {
        BlockRef {
            hash: BlockHash::from([hash; 32]),
            height: Height::try_from(height).expect("height fits"),
        }
    }

    /// A retention floor below every old tip these tests use, so it decides nothing.
    fn floor() -> Height {
        at(90, 0).height
    }

    /// - Old tip still canonical → the new tip descends from it, however many blocks
    ///   arrived at once
    #[test]
    fn extending_the_chain_is_not_a_reorg() {
        let old = at(100, 1);
        assert_eq!(
            classify_tip_change(old, Some(old), floor()),
            TipChange::Advance
        );
    }

    /// - The case the old height comparison dropped entirely: a competing branch wins
    ///   *because* it is longer, so the new tip is higher and 3 blocks were still rewritten
    #[test]
    fn a_reorg_won_by_a_longer_chain_is_counted_with_its_true_depth() {
        assert_eq!(
            classify_tip_change(at(100, 1), Some(at(97, 9)), floor()),
            TipChange::Reorg(Some(3)),
            "100 -> fork at 97 rewrites 3 blocks, whatever height the new tip reached"
        );
    }

    /// - One block replaced by another at the same height rewrites one block, not zero
    #[test]
    fn an_equal_height_swap_rewrites_one_block() {
        assert_eq!(
            classify_tip_change(at(100, 1), Some(at(99, 9)), floor()),
            TipChange::Reorg(Some(1))
        );
    }

    /// - A rollback to a lower canonical tip is a reorg of the distance rolled back
    #[test]
    fn a_rollback_reports_the_distance_lost() {
        assert_eq!(
            classify_tip_change(at(100, 1), Some(at(95, 9)), floor()),
            TipChange::Reorg(Some(5))
        );
    }

    /// - Counted, not measured: the alternative is inventing a depth the window
    ///   cannot see
    #[test]
    fn a_fork_below_the_window_is_a_reorg_of_unknown_depth() {
        assert_eq!(
            classify_tip_change(at(100, 1), None, floor()),
            TipChange::Reorg(None)
        );
    }

    /// An old tip below the retained floor was dropped by the window, not replaced by a branch.
    #[test]
    fn an_old_tip_below_the_retained_floor_is_an_advance() {
        assert_eq!(
            classify_tip_change(at(100, 1), None, at(101, 0).height),
            TipChange::Advance
        );
    }

    /// An old tip at the floor is still retained, so its absence from the chain is a real reorg.
    #[test]
    fn an_old_tip_at_the_retained_floor_is_still_a_reorg() {
        assert_eq!(
            classify_tip_change(at(100, 1), None, at(100, 0).height),
            TipChange::Reorg(None)
        );
    }
}
