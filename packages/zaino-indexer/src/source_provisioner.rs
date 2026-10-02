//! The source-backed provisioner and its streaming driver.
//!
//! The provisioner is the supply half of the indexer (see the design note
//! "provisioner is internal to the indexer"): it fetches blocks from a validator
//! and projects them into the set-wide context the engine consumes. It is
//! **generic over the source** — bound on exactly the `zaino-source` capability
//! traits it needs, so any validator adapter plugs in — and it reacts to a typed
//! source error rather than baking in retry (transient handling is the
//! resilient-source decorator's job, below).
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

use tracing::warn;
use zaino_async::{panic_message, Task, TaskError, TaskName};
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_finality::{DurableWatermark, HorizonReader, Released};
use zaino_primitives::types::{Block, BlockHash, Height, PreIndexCompactBlock};
use zaino_source::{
    GetBlock, GetChainTip, GetPreIndexCompactBlock, SourceError, SubscribeChainTip, TipObservation,
};
use zaino_sync::backend::Backend;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_pipelines::IndexPipelines;
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

/// The run-tuning knobs for a [`SourceSyncDriver`]: how it batches, how much it
/// buffers between provisioner and engine, and how many fetches run
/// concurrently. Grouped into one struct so the three are *named* at every call
/// site rather than passed as a transposition-prone tail of positional numbers.
///
/// Where the sync *boundary* sits is a separate concern — see [`SyncTarget`],
/// passed alongside this.
pub struct SyncTuning {
    /// Blocks committed per atomic engine batch.
    pub batch_size: u32,
    /// Bound on contexts buffered between the provisioner and the engine.
    pub channel_capacity: usize,
    /// How many fetches the provisioner keeps in flight (see [`FetchConcurrency`]).
    pub concurrency: FetchConcurrency,
}

/// Where the driver's sync target comes from.
///
/// In a composed runtime the seam has one owner, so the driver *consumes* the
/// horizon the volatile tier publishes. Standalone — an isolated finalised
/// store, or a benchmark over a non-reorging source — there is no volatile tier
/// to own it, so the depth derivation is honest there and only there.
pub enum SyncTarget {
    /// The composed runtime: the target is the horizon the volatile tier
    /// publishes, and the watermark is published back through the same seam. The
    /// driver reads the horizon from the half; the engine advances the watermark.
    Seam(DurableWatermark),
    /// Standalone: `tip - depth`, with no volatile tier to coordinate with.
    /// Pass `zaino_consensus::MAX_BLOCK_REORG_HEIGHT`, or `0` over a
    /// non-reorging test source.
    Depth {
        /// Depth below the tip treated as still volatile.
        depth: u32,
    },
}

/// The driver's resolved target, after the watermark (if any) has been moved
/// into the engine: the half's publishing capability lives in the engine, while
/// the driver keeps only a cloneable *reader* of the horizon to drive its loop
/// and progress poller.
#[derive(Clone)]
enum DriverTarget {
    /// Seam mode: read the horizon through this reader; the engine owns the
    /// publishing half.
    Seam(HorizonReader),
    /// Standalone depth mode: derive the boundary from the source tip.
    Depth { depth: u32 },
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

/// The failure of a spawned fetch task, as the indexer's worker-failure error.
fn fetch_task_failure(join: tokio::task::JoinError) -> IndexerError {
    const NAME: TaskName = TaskName("provisioner-fetch");
    let failure = if join.is_panic() {
        TaskError::Panicked {
            name: NAME,
            message: panic_message(join.into_panic().as_ref()),
        }
    } else {
        TaskError::Cancelled { name: NAME }
    };
    IndexerError::UnexpectedWorkerFailure(failure)
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

    /// The block hash the fetched item carries.
    ///
    /// Used for the horizon branch check: the hash is in hand one level before
    /// the item is projected into the engine's `Ctx`, so the check needs no bound
    /// on `Ctx` and no second fetch.
    fn item_hash(item: &Self::Item) -> BlockHash;
}

/// Source whole blocks via [`GetBlock`].
pub struct FullBlocks;

impl<S: GetBlock + Send + Sync + 'static> SourceFetch<S> for FullBlocks {
    type Item = Block;

    async fn fetch(source: &S, height: Height) -> Result<Block, IndexerError> {
        source.get_block(height).await.map_err(map_source)
    }

    fn item_hash(item: &Block) -> BlockHash {
        item.header.hash
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

    fn item_hash(item: &PreIndexCompactBlock) -> BlockHash {
        item.hash
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
    /// Shared with every in-flight fetch task, which projects its own item.
    build: Arc<F>,
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
            build: Arc::new(build),
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

    /// The block hash the source serves at `height`, fetched through the same
    /// strategy the range uses. Used for the horizon branch check before a range
    /// tops out at the seam's named horizon.
    pub async fn item_hash_at(&self, height: Height) -> Result<BlockHash, IndexerError> {
        let item = Fetch::fetch(&self.source, height).await?;
        Ok(Fetch::item_hash(&item))
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
    /// Each fetch runs as its own task, and projects its item there. Fetching
    /// is not only I/O: over RPC it decodes a hex payload and deserialises a
    /// block, and the projection walks every transaction. Polled inside this
    /// one task that work would serialise on a single thread however many
    /// fetches were in flight, so the window would bound latency overlap but
    /// never CPU. Spawned, the window bounds both.
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
                let build = Arc::clone(&self.build);
                in_flight.push_back(tokio::spawn(async move {
                    let item = Fetch::fetch(&source, height).await?;
                    Ok::<Ctx, IndexerError>(build(item))
                }));
                next += 1;
            }

            // Drain the oldest fetch — `FuturesOrdered` guarantees this is the
            // lowest outstanding height.
            match in_flight.next().await {
                Some(Ok(Ok(ctx))) => {
                    if tx.send(ctx).await.is_err() {
                        // Receiver dropped: the engine stopped consuming.
                        return Ok(());
                    }
                }
                // First fetch error: stop submitting, drop the in-flight rest.
                Some(Ok(Err(e))) => return Err(e),
                // The fetch task itself died: a panic in the source or the
                // projection, or the runtime shutting down under us.
                Some(Err(join)) => return Err(fetch_task_failure(join)),
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
    /// Where the sync boundary comes from: the seam's horizon (composed) or a
    /// depth below the source tip (standalone). The publishing half, if any, has
    /// been moved into the engine; this keeps only the reader.
    target: DriverTarget,
    channel_capacity: usize,
    /// A read handle onto the same backend the engine writes, for the progress
    /// poller to read the committed watermark (concurrent with the engine's
    /// writer — the persisted watermark is the on-disk truth it reports).
    backend: B,
}

impl<S, B: Backend, Ctx: Send + Sync + 'static, F, Fetch> SourceSyncDriver<S, B, Ctx, F, Fetch> {
    /// A driver syncing from `start` to the boundary [`SyncTarget`] names,
    /// buffering up to `channel_capacity` contexts between the provisioner and the
    /// engine.
    ///
    /// The engine builds only the finalised, append-only range, so bulk sync and
    /// tip-following are one operation and no reorg handling is needed here — the
    /// volatile window above the boundary is the chain-head's concern.
    ///
    /// For [`SyncTarget::Seam`] the watermark half is moved into the engine (which
    /// publishes against the horizon after each batch) and the driver keeps a
    /// reader to drive its loop; for [`SyncTarget::Depth`] the engine holds no
    /// half and the boundary is derived from the source tip.
    pub fn new(
        engine: SyncEngine<Ctx, B>,
        provisioner: Arc<SourceProvisioner<S, Ctx, F, Fetch>>,
        start: Height,
        target: SyncTarget,
        channel_capacity: usize,
        backend: B,
    ) -> Self {
        // In seam mode the publishing half goes to the engine; the driver keeps a
        // cloneable reader of the horizon for its loop and progress poller.
        let (engine, target) = match target {
            SyncTarget::Seam(watermark) => {
                let reader = watermark.reader();
                (
                    engine.with_watermark(Some(watermark)),
                    DriverTarget::Seam(reader),
                )
            }
            SyncTarget::Depth { depth } => {
                (engine.with_watermark(None), DriverTarget::Depth { depth })
            }
        };
        Self {
            engine: Mutex::new(Some(engine)),
            provisioner,
            start,
            target,
            channel_capacity,
            backend,
        }
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
        pipelines: IndexPipelines<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
        target: SyncTarget,
    ) -> Result<Self, IndexerError>
    where
        B: Clone,
    {
        let start = crate::assess_start(backend)?.next_height();
        let engine = SyncEngine::from_pipelines(
            pipelines,
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
            target,
            tuning.channel_capacity,
            backend.clone(),
        ))
    }

    /// Refuses a range whose top block is not the one the seam named.
    ///
    /// Checked where the fetched block's hash is in hand rather than at publish
    /// time: the engine knows when a batch became durable but not which block it
    /// was, and a mismatch must stop the write rather than be reported after
    /// divergent data is on disk.
    fn check_horizon_branch(
        authorised_by: Option<&Released>,
        at: Height,
        served: BlockHash,
    ) -> Result<(), IndexerError> {
        let Some(released) = authorised_by else {
            return Ok(());
        };
        if at != released.height() {
            return Ok(());
        }
        if served == released.hash() {
            return Ok(());
        }
        Err(IndexerError::HorizonBranchMismatch {
            height: at,
            named: released.hash(),
            served,
        })
    }

    /// Provision `[from, to]` through the engine: the provisioner feeds a bounded
    /// channel which the engine drains, then both are joined typed.
    ///
    /// When `authorised_by` names the horizon this range tops out at, the block
    /// the source serves there is verified against the seam's hash **before** any
    /// block is provisioned — so a branch disagreement is refused before anything
    /// is committed, never after divergent data is on disk.
    async fn sync_to(
        &self,
        engine: &mut SyncEngine<Ctx, B>,
        from: Height,
        to: Height,
        authorised_by: Option<&Released>,
    ) -> Result<(), IndexerError> {
        // Pre-flight horizon branch check: if this range tops out at the seam's
        // horizon, the source must serve that exact block. Fetching the top block
        // first costs one extra fetch of a single height, but it is the only point
        // at which a mismatch can be refused before the engine commits any batch.
        if let Some(released) = authorised_by {
            if to == released.height() {
                let served = self.provisioner.item_hash_at(to).await?;
                Self::check_horizon_branch(Some(released), to, served)?;
            }
        }

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

    /// Index up to `target` under `authorised_by`, advancing `next_from` past the
    /// range on success.
    ///
    /// A no-op when `target` is below `next_from` (nothing new). On success the
    /// next range begins just past `target`; the authorisation is handed to the
    /// engine first, so each batch it commits publishes its watermark under the
    /// horizon that authorised the work.
    async fn index_to(
        &self,
        engine: &mut SyncEngine<Ctx, B>,
        next_from: &mut Height,
        target: Height,
        authorised_by: Option<Released>,
    ) -> Result<(), IndexerError> {
        if u32::from(target) < u32::from(*next_from) {
            return Ok(());
        }
        engine.set_authorisation(authorised_by);
        self.sync_to(engine, *next_from, target, authorised_by.as_ref())
            .await?;
        *next_from = target
            .checked_add(1)
            .expect("a target below the protocol limit has a successor");
        Ok(())
    }

    /// The next height to index from after a follow-range failure: just past the
    /// backend's committed watermark, or the unchanged `fallback` if nothing is
    /// committed yet. Lets a follow range resume from durable truth rather than
    /// re-running the whole failed range or taking the runtime down.
    fn resume_from_committed(&self, fallback: Height) -> Height {
        SyncEngine::<Ctx, B>::committed_height(&self.backend)
            .ok()
            .flatten()
            .and_then(|height| u32::try_from(height.value()).ok())
            .and_then(|height| Height::try_from(height).ok())
            .and_then(|height| height.checked_add(1))
            .unwrap_or(fallback)
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
    /// which takes an explicit start, it cannot forget to resume. `target` selects
    /// the boundary — [`SyncTarget::Seam`] in a composed runtime,
    /// [`SyncTarget::Depth`] standalone.
    pub fn resuming(
        backend: &B,
        pipelines: IndexPipelines<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
        target: SyncTarget,
    ) -> Result<Self, IndexerError> {
        Self::assemble_resuming(backend, pipelines, source, build, tuning, target)
    }
}

/// What the compact-block driver needs of a source: the pre-index compact
/// block fetch, the tip, and tip changes — the canonical (resilient) ports a
/// composition root supplies through a
/// [`ValidatorClient`](zaino_source::ValidatorClient). Named here, once, so a
/// root bounds on this rather than restating the list.
pub trait CompactSource:
    GetPreIndexCompactBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static
{
}
impl<S> CompactSource for S where
    S: GetPreIndexCompactBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static
{
}

impl<S, B, Ctx, F> SourceSyncDriver<S, B, Ctx, F, CompactBlocks>
where
    S: CompactSource,
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
        pipelines: IndexPipelines<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
        target: SyncTarget,
    ) -> Result<Self, IndexerError> {
        Self::assemble_resuming(backend, pipelines, source, build, tuning, target)
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
        // with the engine's writer, and the sync boundary as the target.
        let poll_cancel = cancel.child_token();
        let _poll_guard = poll_cancel.clone().drop_guard();
        {
            let backend = self.backend.clone();
            let tip_source = Arc::clone(&self.provisioner);
            let reporter = reporter.clone();
            // The indexer only builds the append-only range below the boundary, so
            // its progress target is that boundary — the horizon (seam) or
            // tip − depth (standalone) — not the live tip. Reporting the live tip
            // would peg it at a chronic 99.9%, never reaching its own caught-up
            // height; the chain-head (NFS) is what tracks the live tip.
            let poll_target = self.target.clone();
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
                            let target = match &poll_target {
                                DriverTarget::Seam(reader) => reader
                                    .released()
                                    .map(|released| u64::from(u32::from(released.height()))),
                                DriverTarget::Depth { depth } => tip_source
                                    .current_tip()
                                    .await
                                    .ok()
                                    .map(|tip| u64::from(u32::from(tip.saturating_sub(*depth)))),
                            };
                            reporter.progress(committed, target);
                        }
                    }
                }
            });
        }

        // The next height to index from: genesis on a fresh backend, or just past
        // the resume point. Advanced past each range as it is built, so nothing is
        // re-indexed and nothing below the start is ever touched.
        let mut next_from = self.start;

        match &self.target {
            DriverTarget::Seam(reader) => {
                let mut reader = reader.clone();
                // Initial catch-up: whatever horizon the volatile tier has already
                // published. None means it has not anchored yet — the follow loop
                // awaits its first advance.
                if let Some(released) = reader.released() {
                    self.index_to(
                        &mut engine,
                        &mut next_from,
                        released.height(),
                        Some(released),
                    )
                    .await?;
                }
                reporter.ready();

                // Steady-state follow: the horizon drives the loop. The volatile
                // tier publishes it when its own tip advances, so the driver needs
                // no tip subscription of its own to know when to build.
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(()),
                        released = reader.await_released() => {
                            if let Err(error) = self
                                .index_to(&mut engine, &mut next_from, released.height(), Some(released))
                                .await
                            {
                                next_from = self.resume_from_committed(next_from);
                                warn!(
                                    %error,
                                    to = u32::from(released.height()),
                                    resumed_at = u32::from(next_from),
                                    "follow sync failed; the finalised index resumes from its \
                                     committed watermark on the next horizon",
                                );
                            }
                        }
                    }
                }
            }
            DriverTarget::Depth { depth } => {
                let depth = *depth;
                // Initial catch-up: sync [start, tip − depth], then Ready. Only the
                // append-only range is built; the volatile window above it is the
                // chain-head's concern.
                let target = self.provisioner.current_tip().await?.saturating_sub(depth);
                self.index_to(&mut engine, &mut next_from, target, None)
                    .await?;
                reporter.ready();

                // Steady-state follow: index each new range as the tip advances. A
                // source that offers no tip subscription cannot be followed: the
                // index stays at the caught-up height for the life of the run,
                // which is a wiring fault (a composite synthesises the subscription
                // by polling), so it is said loudly rather than parked on quietly.
                let Some(mut tips) = self.provisioner.subscribe_tip() else {
                    warn!(
                        next_from = u32::from(next_from),
                        "source offers no tip subscription; the finalised index will not advance \
                         past its caught-up height — wire a tip poller on the validator"
                    );
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
                            // Copy the height out before awaiting (drop the watch
                            // borrow), then cap at the boundary — we only index
                            // append-only.
                            let target = tips.borrow_and_update().height.saturating_sub(depth);
                            if let Err(error) =
                                self.index_to(&mut engine, &mut next_from, target, None).await
                            {
                                // The source could not serve part of the range — a
                                // state cache that has not caught up to the boundary,
                                // or a transport the fallback cannot reach. Whatever
                                // was committed before the failure is durable; resume
                                // from there on the next tip change rather than take
                                // the runtime down for a range the next poll may
                                // serve. Not retried here: the source has already
                                // retried transient failures under its own policy.
                                next_from = self.resume_from_committed(next_from);
                                warn!(
                                    %error,
                                    to = u32::from(target),
                                    resumed_at = u32::from(next_from),
                                    "follow sync failed; the finalised index resumes from its \
                                     committed watermark on the next tip change",
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
