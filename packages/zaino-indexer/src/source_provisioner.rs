//! The source-backed provisioner and its streaming driver.
//!
//! The provisioner is the supply half of the indexer (see the design note
//! "provisioner is internal to the indexer"): it fetches blocks from a validator
//! and projects them into the set-wide context the engine consumes. It is
//! **generic over the source** — bound on exactly the `zaino-source` capability
//! traits it needs, so any validator adapter (zebra-rpc, zebra-readstate, a
//! mock) plugs in — and it reacts to a typed source error rather than baking in
//! retry (transient handling is the resilient-source decorator's job, below).
//!
//! [`SourceSyncDriver`] wires it to the engine: it spawns the provisioner
//! feeding the engine's `sync_channel`, and reports `Ready` once caught up to the
//! source tip. Steady-state tip-following (via `SubscribeChainTip`) is the next
//! increment; this drives the initial sync to the current tip.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use futures::stream::{FuturesOrdered, StreamExt};
use tokio::sync::mpsc;

use tokio::sync::watch;

use zaino_async::{Task, TaskName};
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_primitives::types::{Block, Height, PreIndexCompactBlock};
use zaino_source::{
    GetBlock, GetChainTip, GetPreIndexCompactBlock, SourceError, SubscribeChainTip, TipObservation,
};
use zaino_sync::backend::Backend;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_set::IndexSet;
use zaino_sync::primitives::BlockHeight;

use crate::IndexerError;

/// Name of the provisioner's block-fetch pump task. Spawned as a [`Task`], so a
/// panic/cancel surfaces attributed to *this* name (via [`crate::IndexerError::UnexpectedWorkerFailure`]),
/// never tokio's opaque runtime task id.
const PROVISION_WORKER: TaskName = TaskName("block-provisioner");

/// How many block fetches the provisioner keeps in flight at once.
///
/// A `NonZeroUsize` newtype, so zero is unrepresentable — there is always at
/// least one fetch, and [`provision`](SourceProvisioner::provision) needs no
/// runtime guard. It is a distinct type (not a bare `NonZeroUsize`) so the
/// compiler tells this knob apart from every other count. [`SERIAL`] (one in
/// flight) is deterministic — the choice for tests and mocks; a larger value
/// feeds the rayon-parallel engine rather than pacing it one block at a time.
///
/// [`SERIAL`]: FetchConcurrency::SERIAL
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct FetchConcurrency(NonZeroUsize);

impl FetchConcurrency {
    /// One fetch in flight — serial and deterministic (tests/mocks).
    pub const SERIAL: Self = Self(NonZeroUsize::MIN);

    /// Wrap a non-zero fetch count.
    pub const fn new(count: NonZeroUsize) -> Self {
        Self(count)
    }

    /// The count as a `usize` (always ≥ 1).
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl std::fmt::Display for FetchConcurrency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for FetchConcurrency {
    type Err = <NonZeroUsize as std::str::FromStr>::Err;

    /// Parses a positive integer; rejects `0` (and non-numbers) at the parse
    /// boundary — a CLI/config `concurrency = 0` fails loud, never silently
    /// coerced.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<NonZeroUsize>().map(Self)
    }
}

/// The run-tuning knobs for a [`SourceSyncDriver`]: how it batches, where the
/// finalised boundary sits, how much it buffers between provisioner and engine,
/// and how many fetches run concurrently. Grouped into one struct so the four
/// are *named* at every call site rather than passed as a transposition-prone
/// tail of positional numbers.
pub struct SyncTuning {
    /// Blocks committed per atomic engine batch.
    pub batch_size: u32,
    /// Depth below the tip treated as still volatile; only `tip - depth` and
    /// below is indexed. `zaino_consensus::MAX_BLOCK_REORG_HEIGHT` standalone;
    /// `0` over a non-reorging test source.
    pub finalised_depth: u32,
    /// Bound on contexts buffered between the provisioner and the engine.
    pub channel_capacity: usize,
    /// How many fetches the provisioner keeps in flight (see [`FetchConcurrency`]).
    pub concurrency: FetchConcurrency,
}

/// Map a resilient-port [`SourceError`] onto an indexer error, preserving each
/// cause typed (no stringification). `Unavailable` and `Fetch` are concrete;
/// only the generic `Domain(E)` is boxed, since one non-generic `IndexerError`
/// cannot hold every `E`. No wildcard arm: a new `SourceError` variant must be
/// classified here.
fn map_source<E: std::error::Error + Send + Sync + 'static>(err: SourceError<E>) -> IndexerError {
    match err {
        SourceError::Unavailable(u) => IndexerError::SourceUnreachable(u),
        SourceError::NonDomain(f) => IndexerError::Transport(f),
        SourceError::Domain(d) => IndexerError::Domain(Box::new(d)),
    }
}

/// How the provisioner obtains one per-height unit from the source.
///
/// The strategy selects the fetch capability at the type level: [`FullBlocks`]
/// pulls whole [`Block`]s ([`GetBlock`]); [`CompactBlocks`] pulls the cheaper
/// [`PreIndexCompactBlock`] ([`GetPreIndexCompactBlock`]) that skips proof and
/// signature deserialization. The engine and index set are identical either way —
/// only the fetched item and its projection differ.
pub trait SourceFetch<S>: Send + Sync + 'static {
    /// The per-height item this strategy fetches.
    type Item: Send + 'static;

    /// Fetch the item at `height`.
    fn fetch(
        source: &S,
        height: Height,
    ) -> impl std::future::Future<Output = Result<Self::Item, IndexerError>> + Send;
}

/// Source whole blocks via [`GetBlock`].
pub struct FullBlocks;

impl<S: GetBlock + Send + Sync + 'static> SourceFetch<S> for FullBlocks {
    type Item = Block;

    async fn fetch(source: &S, height: Height) -> Result<Block, IndexerError> {
        source.get_block(height).await.map_err(map_source)
    }
}

/// Source pre-index compact blocks via [`GetPreIndexCompactBlock`] — the fast
/// path that skips proof/signature deserialization. The trees index still gets
/// what it needs: the compact block carries per-tx sapling outputs and orchard
/// actions, which the chain-metadata index counts.
pub struct CompactBlocks;

impl<S: GetPreIndexCompactBlock + Send + Sync + 'static> SourceFetch<S> for CompactBlocks {
    type Item = PreIndexCompactBlock;

    async fn fetch(source: &S, height: Height) -> Result<PreIndexCompactBlock, IndexerError> {
        source
            .get_pre_index_compact_block(height)
            .await
            .map_err(map_source)
    }
}

/// Fetches per-height units from a validator source and projects them into the
/// engine's set-wide context `Ctx` via `build`.
///
/// Generic over the source `S` (capability-bound), the fetch strategy `Fetch`
/// (full or compact blocks), and the projection `build`, so the same provisioner
/// serves any adapter, any source shape, and any index set.
pub struct SourceProvisioner<S, Ctx, F, Fetch> {
    source: Arc<S>,
    build: F,
    /// How many fetches are kept in flight at once (see [`FetchConcurrency`]).
    concurrency: FetchConcurrency,
    _ctx: std::marker::PhantomData<fn() -> Ctx>,
    _fetch: std::marker::PhantomData<fn() -> Fetch>,
}

impl<S, Ctx, F, Fetch> SourceProvisioner<S, Ctx, F, Fetch>
where
    S: GetChainTip + SubscribeChainTip + Send + Sync + 'static,
    Fetch: SourceFetch<S>,
    F: Fn(Fetch::Item) -> Ctx + Send + Sync + 'static,
    Ctx: Send + 'static,
{
    /// A provisioner over `source`, projecting each fetched item with `build`,
    /// keeping up to `concurrency` fetches in flight ([`FetchConcurrency::SERIAL`]
    /// for a deterministic serial path; a concurrent value in production).
    pub fn new(source: Arc<S>, build: F, concurrency: FetchConcurrency) -> Self {
        Self {
            source,
            build,
            concurrency,
            _ctx: std::marker::PhantomData,
            _fetch: std::marker::PhantomData,
        }
    }

    /// The validator's current tip height.
    pub async fn current_tip(&self) -> Result<Height, IndexerError> {
        self.source
            .get_chain_tip()
            .await
            .map(|(_hash, height)| height)
            .map_err(map_source)
    }

    /// A push subscription to the source's tip, or `None` if the source does not
    /// push (in which case the indexer stays at its initial catch-up height).
    pub fn subscribe_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        self.source.subscribe_to_chain_tip()
    }

    /// Fetch `[from, to]` and send each projected context into `tx`, **in
    /// ascending height order**, keeping up to `self.concurrency` fetches in
    /// flight.
    ///
    /// Ordering is load-bearing: the engine's `(SelfCumulative, Append)` indexes
    /// thread a carry in height order, so out-of-order delivery would corrupt
    /// cumulative values. [`FuturesOrdered`] yields results in submission
    /// (height) order regardless of which fetch finishes first, which is what
    /// makes concurrent fetch safe here.
    ///
    /// The in-flight window is bounded by `concurrency` (fetched items buffer
    /// only up to that), and `tx.send` applies channel backpressure. Stops early
    /// with `Ok` if the receiver is dropped (the engine went away); returns the
    /// **first** fetch error and abandons the rest.
    pub async fn provision(
        &self,
        from: Height,
        to: Height,
        tx: mpsc::Sender<Ctx>,
    ) -> Result<(), IndexerError> {
        let from = u32::from(from);
        let to = u32::from(to);
        // `FetchConcurrency` is non-zero by construction — no runtime guard.
        let concurrency = self.concurrency.get();

        let mut in_flight = FuturesOrdered::new();
        let mut next = from;

        loop {
            // Refill the in-flight window with the next heights, in order.
            while in_flight.len() < concurrency && next <= to {
                // `next` lies within `[from, to]`, both valid `Height`s, so the
                // conversion is infallible by construction.
                let height = Height::try_from(next).expect("height within a valid range is valid");
                let source = Arc::clone(&self.source);
                in_flight.push_back(async move { Fetch::fetch(&source, height).await });
                next += 1;
            }

            // Drain the oldest fetch — `FuturesOrdered` guarantees this is the
            // lowest outstanding height.
            match in_flight.next().await {
                Some(Ok(item)) => {
                    let ctx = (self.build)(item);
                    if tx.send(ctx).await.is_err() {
                        // Receiver dropped: the engine stopped consuming.
                        return Ok(());
                    }
                }
                // First fetch error: stop submitting, drop the in-flight rest.
                Some(Err(e)) => return Err(e),
                // Window empty and nothing left to submit: the range is done.
                None => return Ok(()),
            }
        }
    }
}

/// Drives a [`SyncEngine`] from a [`SourceProvisioner`], presented as a
/// [`RunLoop`]. The provisioner streams into the engine's `sync_channel`; the
/// component reaches `Ready` once the engine has consumed up to the source tip.
pub struct SourceSyncDriver<S, B: Backend, Ctx, F, Fetch> {
    engine: Mutex<Option<SyncEngine<Ctx, B>>>,
    provisioner: Arc<SourceProvisioner<S, Ctx, F, Fetch>>,
    start: Height,
    finalised_depth: u32,
    channel_capacity: usize,
    /// A read handle onto the same backend the engine writes, for the progress
    /// poller to read the committed watermark (concurrent with the engine's
    /// writer — the persisted watermark is the on-disk truth it reports).
    backend: B,
}

impl<S, B: Backend, Ctx, F, Fetch> SourceSyncDriver<S, B, Ctx, F, Fetch> {
    /// A driver syncing from `start` to the **finalised boundary**, buffering up
    /// to `channel_capacity` contexts between the provisioner and the engine.
    ///
    /// The engine builds only the finalised, append-only range, so bulk sync and
    /// tip-following are one operation and no reorg handling is needed here — the
    /// volatile window above the boundary is the chain-head's concern.
    ///
    /// **`finalised_depth` is the *standalone* seam derivation** (`tip − depth`,
    /// for an indexer with no chain-head — e.g. a benchmark or an isolated
    /// finalised store). In the composed runtime the seam has a single owner —
    /// the chain-head's floor — which the indexer must *consume*, not re-derive,
    /// so FS-ceiling and NFS-floor cannot drift (see the design decision
    /// "the seam has one owner"). That path replaces this depth with the
    /// chain-head's published seam when the chain-head is wired in.
    /// Pass `zaino_consensus::MAX_BLOCK_REORG_HEIGHT` standalone; `0` in tests
    /// over a non-reorging source.
    pub fn new(
        engine: SyncEngine<Ctx, B>,
        provisioner: Arc<SourceProvisioner<S, Ctx, F, Fetch>>,
        start: Height,
        finalised_depth: u32,
        channel_capacity: usize,
        backend: B,
    ) -> Self {
        Self {
            engine: Mutex::new(Some(engine)),
            provisioner,
            start,
            finalised_depth,
            channel_capacity,
            backend,
        }
    }

    /// The finalised boundary for a given source tip: `tip − finalised_depth`,
    /// saturating at genesis. Append-only, so it is a safe sync target.
    fn finalised(&self, tip: Height) -> Height {
        tip.saturating_sub(self.finalised_depth)
    }
}

impl<S, B, Ctx, F, Fetch> SourceSyncDriver<S, B, Ctx, F, Fetch>
where
    S: GetChainTip + SubscribeChainTip + Send + Sync + 'static,
    B: Backend + Send + Sync + 'static,
    Ctx: Send + Sync + 'static,
    Fetch: SourceFetch<S>,
    F: Fn(Fetch::Item) -> Ctx + Send + Sync + 'static,
{
    /// Shared resume-safe assembly for the [`resuming`](Self::resuming) (full
    /// blocks) and [`resuming_compact`](Self::resuming_compact) (pre-index compact
    /// blocks) constructors, which differ only in the fetch strategy.
    ///
    /// Reads the backend's watermark to decide where to start and sets *both* the
    /// engine's start height and the driver's start to match, so a restart
    /// resumes rather than re-indexing from genesis. `backend` is borrowed to
    /// assess it and cloned into the engine, so the caller keeps its handle (e.g.
    /// to hand the same backend to the store). Whether the persisted indexes are
    /// *compatible* is a separate concern: the engine rejects an incompatible
    /// index while loading state here.
    fn assemble_resuming(
        backend: &B,
        index_set: IndexSet<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
    ) -> Result<Self, IndexerError>
    where
        B: Clone,
    {
        let start = crate::assess_start(backend)?.next_height();
        let engine = SyncEngine::from_index_set(
            index_set,
            backend.clone(),
            EngineConfig {
                batch_size: tuning.batch_size,
                start_height: BlockHeight::new(u64::from(start)),
            },
        )?;
        let provisioner = Arc::new(SourceProvisioner::<S, Ctx, F, Fetch>::new(
            source,
            build,
            tuning.concurrency,
        ));
        Ok(Self::new(
            engine,
            provisioner,
            start,
            tuning.finalised_depth,
            tuning.channel_capacity,
            backend.clone(),
        ))
    }

    /// Provision `[from, to]` through the engine: the provisioner feeds a bounded
    /// channel which the engine drains, then both are joined typed.
    async fn sync_to(
        &self,
        engine: &mut SyncEngine<Ctx, B>,
        from: Height,
        to: Height,
    ) -> Result<(), IndexerError> {
        let (tx, rx) = mpsc::channel(self.channel_capacity);
        let provisioner = Arc::clone(&self.provisioner);
        // The pump is bounded (fetch `[from, to]` then end), so it ignores the
        // cooperative-cancel token; it stops on completion or on the receiver
        // dropping (channel close).
        let pump = Task::spawn(PROVISION_WORKER, move |_cancel| async move {
            provisioner.provision(from, to, tx).await
        });
        // Dropping `tx` (moved into the pump) at its end closes the channel, so
        // `sync_channel` returns once the range is drained.
        engine.sync_channel(rx).await?;
        // Join the pump: the outer `?` turns a panic/cancel into a named
        // `UnexpectedWorkerFailure` (via `From<TaskError>`) — never a raw tokio id; the
        // inner `?` propagates a provisioning error the pump returned normally.
        pump.join().await??;
        Ok(())
    }
}

impl<S, B, Ctx, F> SourceSyncDriver<S, B, Ctx, F, FullBlocks>
where
    S: GetBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static,
    B: Backend + Clone + Send + Sync + 'static,
    Ctx: Send + Sync + 'static,
    F: Fn(Block) -> Ctx + Send + Sync + 'static,
{
    /// A resume-safe driver that sources **whole blocks** ([`GetBlock`]).
    ///
    /// The constructor a runtime bringup should use: unlike [`new`](Self::new),
    /// which takes an explicit start, it cannot forget to resume. Pass
    /// `zaino_consensus::MAX_BLOCK_REORG_HEIGHT` as `finalised_depth` standalone;
    /// `0` in tests over a non-reorging source.
    pub fn resuming(
        backend: &B,
        index_set: IndexSet<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
    ) -> Result<Self, IndexerError> {
        Self::assemble_resuming(backend, index_set, source, build, tuning)
    }
}

impl<S, B, Ctx, F> SourceSyncDriver<S, B, Ctx, F, CompactBlocks>
where
    S: GetPreIndexCompactBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static,
    B: Backend + Clone + Send + Sync + 'static,
    Ctx: Send + Sync + 'static,
    F: Fn(PreIndexCompactBlock) -> Ctx + Send + Sync + 'static,
{
    /// A resume-safe driver that sources the cheaper **pre-index compact blocks**
    /// ([`GetPreIndexCompactBlock`]) — the fast path that skips proof/signature
    /// deserialization. Same resume semantics as [`resuming`](Self::resuming); the
    /// trees index still gets its commitment counts from the compact block.
    pub fn resuming_compact(
        backend: &B,
        index_set: IndexSet<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
    ) -> Result<Self, IndexerError> {
        Self::assemble_resuming(backend, index_set, source, build, tuning)
    }
}

impl<S, B, Ctx, F, Fetch> RunLoop for SourceSyncDriver<S, B, Ctx, F, Fetch>
where
    S: GetChainTip + SubscribeChainTip + Send + Sync + 'static,
    B: Backend + Clone + Send + Sync + 'static,
    Ctx: Send + Sync + 'static,
    Fetch: SourceFetch<S>,
    F: Fn(Fetch::Item) -> Ctx + Send + Sync + 'static,
{
    type Error = IndexerError;
    const LABEL: &'static str = "run loop";
    const RUNNING: Lifecycle = Lifecycle::Syncing;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), IndexerError> {
        let mut engine = self
            .engine
            .lock()
            .expect("engine mutex poisoned")
            .take()
            .ok_or(IndexerError::AlreadyRun)?;

        // Self-report committed-watermark progress on a ~1s tick, for the run's
        // lifetime — the drop guard cancels the poller when `run` returns. The
        // poller reads the *persisted* watermark (the on-disk truth) concurrently
        // with the engine's writer, and the source tip as the target.
        let poll_cancel = cancel.child_token();
        let _poll_guard = poll_cancel.clone().drop_guard();
        {
            let backend = self.backend.clone();
            let tip_source = Arc::clone(&self.provisioner);
            let reporter = reporter.clone();
            let _poller = Task::spawn(TaskName("indexer-progress"), move |_unused| async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    tokio::select! {
                        _ = poll_cancel.cancelled() => break,
                        _ = ticker.tick() => {
                            let committed = backend
                                .reader()
                                .ok()
                                .and_then(|reader| {
                                    zaino_persistence_codec::watermark::read(&reader)
                                        .ok()
                                        .flatten()
                                })
                                .map(|height| u64::from(u32::from(height)))
                                .unwrap_or(0);
                            let target = tip_source
                                .current_tip()
                                .await
                                .ok()
                                .map(|height| u64::from(u32::from(height)));
                            reporter.progress(committed, target);
                        }
                    }
                }
            });
        }

        // Initial catch-up: sync [start, finalised-boundary], then Ready. Only
        // the append-only finalised range is built; the volatile window above it
        // is the chain-head's concern.
        let mut synced = self.finalised(self.provisioner.current_tip().await?);
        if u32::from(synced) >= u32::from(self.start) {
            self.sync_to(&mut engine, self.start, synced).await?;
        }
        reporter.ready();

        // Steady-state follow: index each new range as the tip advances. If the
        // source does not push a tip (`None`), stay at the caught-up height.
        let Some(mut tips) = self.provisioner.subscribe_tip() else {
            cancel.cancelled().await;
            return Ok(());
        };
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                changed = tips.changed() => {
                    if changed.is_err() {
                        // The source stopped publishing; nothing more to follow.
                        return Ok(());
                    }
                    // Copy the height out before awaiting (drop the watch borrow),
                    // then cap at the finalised boundary — we only index append-only.
                    let tip = self.finalised(tips.borrow_and_update().height);
                    if tip > synced {
                        let from = synced
                            .checked_add(1)
                            .expect("tip below max height has a successor");
                        self.sync_to(&mut engine, from, tip).await?;
                        synced = tip;
                    }
                }
            }
        }
    }
}
