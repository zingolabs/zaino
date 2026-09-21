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
//! Everything else — extending one block at a time, the reorg walk, the
//! non-higher reorg check, best-block selection by accumulated work, and
//! trimming with its keep-the-highest rule — reconciles the two into a graph
//! driven only through the [`ChainGraph`] moves `extend` and `rewind_to`.
//!
//! # Construction and the writer are separate
//!
//! [`anchor`](ChainHeadService::anchor) builds a complete window and returns a
//! read handle (a [`ChainHeadSubscriber`]) alongside the runnable writer. The
//! writer is a [`RunLoop`]: the runtime boots it as a supervised `RunComponent`,
//! exactly as it does the finalised indexer, so the chain-head escalates and is
//! supervised the same way. This mirrors the finalised side — a readable handle
//! composed into the served view, plus a run loop the Orchestra drives — rather
//! than the chain-head spawning and owning its own task.
//!
//! # Advancing is not an operation
//!
//! There is no `sync`, `update` or `reconcile` here at any visibility. The
//! writer loop is the only thing that advances the graph, and it does so
//! through private methods that build a *new* snapshot and hand it to
//! [`publish_snapshot`](ChainHeadService::publish_snapshot). Nothing else can
//! reach the published cell, so a reader can never observe a half-applied
//! reorg or a partially-extended window.

use std::{sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use zaino_chain_head::{
    ChainHeadBlock, ChainHeadBlockSource, ChainHeadConfig, ChainHeadSnapshot as _, ChainHeadWork,
};
use zaino_component::{Lifecycle, RunLoop, RunReporter};
use zaino_primitives::types::{BlockHash, BlockRef, ChainStateEpoch, Height, TreeRoots};
use zaino_status::{NamedAtomicStatus, Status, StatusType};

use crate::{
    error::{ChainHeadAdvanceError, ChainHeadInitError},
    graph::ChainGraph,
    snapshot::MapBackedSnapshot,
    subscriber::ChainHeadSubscriber,
};

/// The name this component reports status under.
const COMPONENT: &str = "ChainHead";

/// Confirmation overlap kept below the finalised store's confirmed watermark.
///
/// The trim floor stops this far below the confirmed watermark rather than
/// exactly at it, so the non-finalised and finalised windows overlap by a few
/// blocks. The overlap is what closes the seam: even if the two sides observe
/// the boundary height a tick apart, no height is ever below the non-finalised
/// floor and above the finalised watermark at the same time, so none falls in a
/// gap served by neither.
const RETENTION_MARGIN: u32 = 10;

/// The bounded non-finalised head of the chain, kept current with a validator.
///
/// The *writer*: it advances the graph. It is anchored by
/// [`anchor`](Self::anchor) and then driven as a [`RunLoop`] by a runtime
/// `RunComponent`. Everything else holds a [`ChainHeadSubscriber`], which reads
/// published snapshots and nothing else.
pub struct ChainHeadService<S: ChainHeadBlockSource> {
    /// We need access to the validator's best block hash, as well as a source
    /// of blocks.
    source: Arc<S>,
    /// This lock should not be exposed to consumers. Rather, clone the Arc and
    /// offer that. This means we can overwrite the arc without interfering with
    /// readers, who will hold a stale copy.
    current: Arc<ArcSwap<MapBackedSnapshot>>,
    updates: watch::Sender<ChainStateEpoch>,
    /// The finalised store's confirmed watermark: the highest height it has
    /// durably committed, or `None` when it holds nothing (an empty or young
    /// chain). Read at trim time so the non-finalised floor never rises above
    /// what the finalised side can already serve.
    confirmed_watermark: watch::Receiver<Option<Height>>,
    status: NamedAtomicStatus,
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
    /// Anchors the graph and returns a read handle alongside the runnable writer.
    ///
    /// Anchoring is the old `initialize` with `resolve_anchor_block`: one block
    /// at the anchor height, which the writer then extends one block at a time.
    /// Doing it before returning is what makes `ChainHeadSubscriber::current`
    /// total — there is no state in which a ChainHead exists with nothing to
    /// answer from.
    ///
    /// The returned [`ChainHeadSubscriber`] is the read handle (composed into the
    /// served chain view); the returned `Self` is the writer, which the runtime
    /// boots as a [`RunLoop`]-driven `RunComponent`. The subscriber shares the
    /// same published cell and status the writer publishes into, so it observes
    /// every later advance without holding the writer.
    ///
    /// `cancel` governs only the anchoring retry here; the run loop is cancelled
    /// through the token its `RunComponent` hands [`RunLoop::run`]. Pass a token
    /// that is a *child* of the runtime's, so runtime shutdown reaches anchoring.
    #[instrument(name = "ChainHeadService::anchor", skip_all, fields(max_depth = config.max_depth()))]
    pub async fn anchor(
        source: Arc<S>,
        config: ChainHeadConfig,
        confirmed_watermark: watch::Receiver<Option<Height>>,
        cancel: CancellationToken,
    ) -> Result<(ChainHeadSubscriber, Self), ChainHeadInitError> {
        let writer = Self::anchored(source, config, confirmed_watermark, &cancel).await?;
        let subscriber = writer.subscriber();
        Ok((subscriber, writer))
    }

    /// An anchored service, wrapped in an `Arc` and with **no writer running**,
    /// for tests that step it.
    ///
    /// Compiled out of production builds. Pair with
    /// [`advance_once`](Self::advance_once): with no writer running, a stepping
    /// test is the only thing advancing the graph, so what it observes is
    /// exactly what it caused.
    ///
    /// Shares `anchored` with [`anchor`](Self::anchor), so the two construction
    /// paths cannot drift — they differ only in whether the writer is driven.
    #[cfg(any(test, feature = "testing"))]
    pub async fn spawn_without_writer(
        source: Arc<S>,
        config: ChainHeadConfig,
        confirmed_watermark: watch::Receiver<Option<Height>>,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ChainHeadInitError> {
        Ok(Arc::new(
            Self::anchored(source, config, confirmed_watermark, &cancel).await?,
        ))
    }

    /// Advances the graph by one iteration and publishes the result.
    ///
    /// Compiled out of production builds. This is what the writer does per tick;
    /// exposing it to tests lets them assert on a specific reorg shape without
    /// racing a timer.
    #[cfg(any(test, feature = "testing"))]
    pub async fn advance_once(&self) -> Result<(), ChainHeadAdvanceError> {
        self.tick().await
    }

    /// Everything [`anchor`](Self::anchor) does except hand back the subscriber:
    /// anchor the graph and build the writer.
    async fn anchored(
        source: Arc<S>,
        config: ChainHeadConfig,
        confirmed_watermark: watch::Receiver<Option<Height>>,
        cancel: &CancellationToken,
    ) -> Result<Self, ChainHeadInitError> {
        let status = NamedAtomicStatus::new(COMPONENT, StatusType::Syncing);

        let snapshot = anchor_with_retry(&source, &config, cancel).await?;
        info!(
            height = u32::from(snapshot.best_tip().height),
            hash = %snapshot.best_tip().hash,
            "ChainHead anchored"
        );

        let (updates, _) = watch::channel(ChainStateEpoch {
            generation: 0,
            best_tip: snapshot.best_tip(),
        });

        // Still `Syncing`: the anchor is the window's floor, not its tip, so a
        // reader served now would see a head up to `max_depth` below the
        // chain. `Ready` is published by the first successful advance, which
        // is the first moment the snapshot matches the validator's tip.
        Ok(Self {
            source,
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
            updates,
            confirmed_watermark,
            status,
            config,
        })
    }

    /// A read-only handle onto the published snapshot.
    ///
    /// The status cell is cloned, not read: the handle observes every later
    /// transition rather than the value that happened to hold here.
    pub fn subscriber(&self) -> ChainHeadSubscriber {
        ChainHeadSubscriber::new(
            Arc::clone(&self.current),
            self.updates.subscribe(),
            self.status.clone(),
        )
    }

    /// The runtime's current status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Publishes `Closing` on the status cell, for a consumer that stops the
    /// writer out of band (the legacy `zaino-state` index owns its own cancel
    /// token and cancels it directly).
    ///
    /// This does **not** stop the writer — the run loop is stopped by cancelling
    /// the token its `RunComponent` (or its self-driver) handed
    /// [`RunLoop::run`]. It only makes the shutdown observable on every handle:
    /// the run loop also publishes `Closing` when it exits on cancellation, but
    /// a synchronous consumer wants the transition visible immediately rather
    /// than after the loop next wakes.
    pub fn shutdown(&self) {
        self.status.store(StatusType::Closing);
    }

    /// One turn of the writer loop's wait: the poll interval, cancellation, or
    /// the source saying it has new blocks.
    ///
    /// The wake is a latency hint and nothing more: it carries no payload, and
    /// the next iteration re-reads the source regardless.
    async fn wait_for_work(
        &self,
        wake: &mut Option<watch::Receiver<()>>,
        cancel: &CancellationToken,
    ) -> std::ops::ControlFlow<()> {
        match wake {
            Some(rx) => tokio::select! {
                _ = cancel.cancelled() => std::ops::ControlFlow::Break(()),
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
            None => sleep_or_cancel(self.config.poll_interval(), cancel).await,
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
                let prev_block = graph.tip_block().clone();
                let chainblock = self.block_to_chainblock(&prev_block, &block).await?;
                info!(
                    height = u32::from(chainblock.height()),
                    hash = %chainblock.hash(),
                    "Syncing block"
                );
                graph.extend(chainblock).map_err(|error| {
                    ChainHeadAdvanceError::ReorgFailure(format!(
                        "extending to a block the validator served: {error}"
                    ))
                })?;
            } else {
                // There's been a reorg. The fresh block is the new chaintip; we
                // work backwards from it and update heights_to_hashes with it
                // and all its parents.
                self.handle_reorg(&mut graph, &block).await?;
            }
        }

        self.check_for_nonhigher_reorgs(&mut graph).await?;

        // Trim floor: retain every height at or above it, drop below. Two
        // independent floors are computed and the lower — the one that retains
        // more — wins:
        //
        // - the reorg-safety floor keeps the whole consensus reorg window, so a
        //   reorg can always be walked back to its fork point regardless of what
        //   the finalised store has confirmed.
        // - the confirmation floor keeps everything the finalised store has not
        //   yet durably confirmed, less the retention overlap. With no
        //   confirmed watermark the finalised side holds nothing, so this floor
        //   is genesis and nothing below the reorg window is dropped.
        //
        // The confirmation floor is the seam invariant: this floor never rises
        // above the finalised store's confirmed watermark minus the retention
        // overlap, so no height is ever below the non-finalised floor and above
        // the finalised watermark at once — the seam between the two never gaps.
        let reorg_safety_floor = height_below(graph.best_tip().height, self.config.max_depth());
        let confirmation_floor = match *self.confirmed_watermark.borrow() {
            Some(watermark) => height_below(watermark, RETENTION_MARGIN),
            None => Height::GENESIS,
        };
        graph.remove_finalized_blocks(reorg_safety_floor.min(confirmation_floor));

        // Best chain is the most-work branch retained, which a reorg may have
        // left as something other than the block we just extended to.
        //
        // Strictly more work, not merely equal: two blocks at one height with
        // the same difficulty carry the same accumulated work, and picking
        // between them by which the map happened to yield last would let a tie
        // flip the tip away from the block the validator just told us is
        // canonical. On a tie the validator's answer — which the walk above has
        // already applied — wins.
        let tip_work = graph.tip_block().work;
        let heaviest = graph.heaviest_block().clone();
        if heaviest.work > tip_work {
            self.handle_reorg(&mut graph, &heaviest).await?;
        }

        Ok(graph)
    }

    /// Handle a blockchain reorg by finding the common ancestor.
    ///
    /// Descends by parent hash from `block` to the first block the graph already
    /// holds on its best chain — the fork point — collecting the branch in
    /// between. It then makes the fork point the tip with a single
    /// [`rewind_to`](ChainGraph::rewind_to) and lays the branch back down
    /// oldest-first with [`extend`](ChainGraph::extend), so work accumulates
    /// from the block below each time. `block` itself is extended last and
    /// returned as the new tip.
    ///
    /// A `rewind_to` or `extend` refusal here is not normal flow: the descent
    /// established the fork point and the branch order, so a refusal means the
    /// source or the graph is inconsistent, and it is reported as
    /// [`ChainHeadAdvanceError::ReorgFailure`].
    async fn handle_reorg(
        &self,
        graph: &mut MapBackedSnapshot,
        block: &impl Block,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let mut branch = Vec::new();
        let mut parent_hash = block.parent_hash();
        let mut descended = 0u32;
        // The walk is bounded by the consensus reorg limit, not by retention: a
        // reorg cannot be deeper than the configured depth however much the
        // graph happens to retain below it.
        let fork_point = loop {
            if descended > self.config.max_depth() {
                return Err(ChainHeadAdvanceError::ReorgFailure(
                    "reorg handling walked beyond the consensus reorg limit".to_string(),
                ));
            }
            descended = descended.saturating_add(1);

            match graph.block_by_hash(&parent_hash).cloned() {
                Some(prev) if graph.is_on_best_chain(prev.reference) => break prev,
                Some(prev) => {
                    parent_hash = prev.parent_hash;
                    branch.push(BranchBlock::Retained(Box::new(prev)));
                }
                None => {
                    let fetched = self.block_at_hash(parent_hash).await?.ok_or_else(|| {
                        ChainHeadAdvanceError::InconsistentSource(format!(
                            "validator is missing block {parent_hash}, the parent of one it served"
                        ))
                    })?;
                    parent_hash = fetched.header.prev_hash;
                    branch.push(BranchBlock::Fetched(Box::new(fetched)));
                }
            }
        };

        graph.rewind_to(fork_point.reference).map_err(|error| {
            ChainHeadAdvanceError::ReorgFailure(format!(
                "rewinding to fork point {}: {error}",
                fork_point.hash()
            ))
        })?;

        let mut prev_block = fork_point;
        for pending in branch.into_iter().rev() {
            prev_block = pending.to_chain_head_block(&prev_block, self).await?;
            graph.extend(prev_block.clone()).map_err(|error| {
                ChainHeadAdvanceError::ReorgFailure(format!(
                    "laying down a reorg branch block: {error}"
                ))
            })?;
        }

        let chainblock = block.to_chain_head_block(&prev_block, self).await?;
        graph.extend(chainblock.clone()).map_err(|error| {
            ChainHeadAdvanceError::ReorgFailure(format!("extending to the reorg tip: {error}"))
        })?;
        Ok(chainblock)
    }

    /// Catches a reorg that did not raise the tip.
    ///
    /// The extension loop only notices a reorg when it finds a *higher* block
    /// whose parent it does not hold. A branch swap at the same height, or a
    /// rollback, produces no such block — this is what sees those.
    ///
    /// It steps down one height at a time from the tip until the source can
    /// serve a block, then reorgs to it if it differs from the tip. Bounded by
    /// the consensus reorg limit: a source that cannot serve within it is
    /// inconsistent.
    async fn check_for_nonhigher_reorgs(
        &self,
        graph: &mut MapBackedSnapshot,
    ) -> Result<(), ChainHeadAdvanceError> {
        let tip_height = graph.best_tip().height;
        let mut target_height = tip_height;
        loop {
            if u32::from(target_height).saturating_add(self.config.max_depth())
                < u32::from(tip_height)
            {
                return Err(ChainHeadAdvanceError::ReorgFailure(
                    "reorg detection walked beyond the consensus reorg limit".to_string(),
                ));
            }

            match self.block_at_height(target_height).await? {
                Some(block) => {
                    if block.header.hash != graph.best_tip().hash {
                        self.handle_reorg(graph, &block).await?;
                    }
                    return Ok(());
                }
                None => {
                    // The source cannot serve this height. Walk down until it
                    // can, bounded by the consensus reorg limit above.
                    if u32::from(target_height) == 0 {
                        return Ok(());
                    }
                    target_height = height_below(target_height, 1);
                }
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
            // Absent, not failed. Matched by name, not a wildcard, so a future
            // second domain variant is caught by the compiler here rather than
            // silently reclassified as absent.
            Err(zaino_source::QueryError::Domain(zaino_source::GetBlockByHashError::NotFound(
                missing,
            ))) => {
                debug!(hash = %missing, "block_at_hash: source reports no block; treating as absent");
                Ok(None)
            }
            Err(error) => Err(ChainHeadAdvanceError::SourceUnavailable(error.to_string())),
        }
    }
}

impl<S: ChainHeadBlockSource> Status for ChainHeadService<S> {
    fn status(&self) -> StatusType {
        self.status.load()
    }
}

impl<S: ChainHeadBlockSource> RunLoop for ChainHeadService<S> {
    type Error = ChainHeadAdvanceError;
    const LABEL: &'static str = "chain-head";
    // A writer, not a server: it already serves dependents (its anchored window)
    // while it catches up, so the running-but-not-yet-`Ready` phase is `Syncing`.
    const RUNNING: Lifecycle = Lifecycle::Syncing;

    /// The writer loop.
    ///
    /// The loop ChainIndex's sync worker ran for the non-finalised state, with
    /// the same backoff ladder, reshaped as a supervised [`RunLoop`]: it reports
    /// `Ready` on its first successful advance (the first moment the published
    /// snapshot matches the validator tip), reports progress as the tip advances,
    /// and runs until `cancel`. A clean cancellation returns `Ok(())` (the
    /// component settles `Offline`); giving up on the validator after
    /// `max_consecutive_failures` returns the last error as `Err` (the component
    /// flips `Critical` and the Orchestra escalates), rather than silently
    /// parking on a stale snapshot.
    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), ChainHeadAdvanceError> {
        let mut wake = self.source.subscribe_to_blocks_received();
        let mut backoff = self.config.initial_backoff();
        let mut consecutive_failures = 0u32;
        let mut announced_ready = false;

        loop {
            if cancel.is_cancelled() {
                self.status.store(StatusType::Closing);
                return Ok(());
            }

            let iteration = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.status.store(StatusType::Closing);
                    return Ok(());
                }
                result = self.tick() => result,
            };

            match iteration {
                Ok(()) => {
                    consecutive_failures = 0;
                    backoff = self.config.initial_backoff();
                    // The first successful advance is the ready condition: the
                    // window now reaches the validator tip. `Ready` on the
                    // status cell is already published from inside `tick`; this
                    // additionally tells the component (idempotent after the
                    // first call).
                    if !announced_ready {
                        reporter.ready();
                        announced_ready = true;
                    }
                    // Progress: the published tip is, after a successful tick,
                    // the tip this iteration read — so current and target agree.
                    let tip = u64::from(u32::from(self.current.load().best_tip().height));
                    reporter.progress(tip, Some(tip));
                    if self.wait_for_work(&mut wake, &cancel).await.is_break() {
                        self.status.store(StatusType::Closing);
                        return Ok(());
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
                        return Err(error);
                    }
                    warn!(%error, attempts = consecutive_failures, "ChainHead failed to advance; retrying");
                    self.status.apply(|s| next_status(s, TickOutcome::Retrying));
                    if sleep_or_cancel(backoff, &cancel).await.is_break() {
                        self.status.store(StatusType::Closing);
                        return Ok(());
                    }
                    backoff = (backoff * 2).min(self.config.max_backoff());
                }
            }
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
    let block_work = std::num::NonZeroU128::from(block.header.bits.to_work()).get();

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

/// A block on the branch a reorg walk descends: either one the graph already
/// retained, or one fetched from the source because the graph did not hold it.
///
/// Boxed per variant because the two block types differ widely in size, and the
/// walk holds a `Vec` of these.
enum BranchBlock {
    Retained(Box<ChainHeadBlock>),
    Fetched(Box<zaino_primitives::types::Block>),
}

impl Block for BranchBlock {
    fn parent_hash(&self) -> BlockHash {
        match self {
            BranchBlock::Retained(block) => block.parent_hash(),
            BranchBlock::Fetched(block) => block.parent_hash(),
        }
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        match self {
            BranchBlock::Retained(block) => block.to_chain_head_block(prev_block, service).await,
            BranchBlock::Fetched(block) => block.to_chain_head_block(prev_block, service).await,
        }
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
