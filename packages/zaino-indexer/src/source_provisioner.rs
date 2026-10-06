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

use tracing::{info, warn};
use zaino_async::{panic_message, Task, TaskError, TaskName};
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_primitives::types::{Block, Height, PreIndexCompactBlock};
use zaino_source::{
    GetBlock, GetChainTip, GetPreIndexCompactBlock, SourceError, SubscribeChainTip, TipObservation,
};
use zaino_sync::backend::{Backend, BulkPolicy};
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

/// How large the initial catch-up gap must be before an `auto` policy brackets
/// it in the backend's bulk mode.
///
/// Below this many blocks the scattered indexes fit their B-trees cheaply and a
/// run-log round trip would cost more than it saves; above it the random-insert
/// write amplification dominates (see the design note "deferred scattered
/// writes"). The value is a measured-pending guess — the spec flags it as such —
/// kept in one place so a cluster A/B can move it deliberately.
pub const DEFER_THRESHOLD_BLOCKS: u32 = 50_000;

/// Whether the indexer may bracket its initial catch-up in the backend's bulk
/// mode, deferring scattered-key writes to sorted run logs for a faster first
/// sync.
///
/// A deployment policy, distinct from the storage *fact* a codec states
/// ([`KeyOrder`](zaino_sync::backend::KeyOrder)) and from the backend's own
/// decision to honour it: the fact enables deferral, the backend may ignore it,
/// and this says whether the deployment wants it at all. `Off` reproduces the
/// direct write path exactly. An already-pending bulk load (a previous run
/// crashed mid-catch-up) is always completed, whatever this says — leaving a
/// namespace unreadable is never a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferralPolicy {
    /// Defer scattered writes when the catch-up gap is large enough to pay off
    /// (at least [`DEFER_THRESHOLD_BLOCKS`]). The default.
    #[default]
    Auto,
    /// Never defer: every commit lands directly, exactly as without this feature.
    Off,
}

impl DeferralPolicy {
    /// Whether this policy permits deferral at all (ignoring the gap threshold).
    pub const fn enabled(self) -> bool {
        matches!(self, DeferralPolicy::Auto)
    }
}

/// Whether the initial catch-up should run inside the backend's bulk mode.
///
/// Bulk mode is entered when a previous run left an unfinished bulk load
/// (`pending`) — re-entered regardless of the gap, so the deferred namespaces
/// are completed even when nothing remains to sync — or when the policy permits
/// deferral and the catch-up `gap` is at least `threshold`. An `Off` policy with
/// nothing pending never enters bulk, so the write path is byte-for-byte the
/// direct one.
fn enters_bulk(policy: DeferralPolicy, pending: bool, gap: u32, threshold: u32) -> bool {
    pending || (policy.enabled() && gap >= threshold)
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
    finalised_depth: u32,
    channel_capacity: usize,
    /// A read handle onto the same backend the engine writes, for the progress
    /// poller to read the committed watermark (concurrent with the engine's
    /// writer — the persisted watermark is the on-disk truth it reports).
    backend: B,
    /// The engine's confirmed-watermark publisher, captured before the engine is
    /// moved behind the run-once mutex. Handed to the non-finalised chain-head so
    /// it can gate trimming on what the finalised store has confirmed
    /// (confirm-before-trim). Exposed via
    /// [`subscribe_confirmed_watermark`](Self::subscribe_confirmed_watermark).
    confirmed_watermark: watch::Receiver<Option<Height>>,
    /// Whether the initial catch-up may be bracketed in the backend's bulk mode.
    /// Defaults to [`DeferralPolicy::Auto`]; a composition root overrides it from
    /// config via [`with_deferral`](Self::with_deferral).
    deferral: DeferralPolicy,
    /// The catch-up gap, in blocks, at or above which an `auto` policy enters
    /// bulk mode. Defaults to [`DEFER_THRESHOLD_BLOCKS`]; carried as a field so a
    /// test can drive the trigger over a small range.
    defer_threshold: u32,
}

impl<S, B: Backend, Ctx: Send + Sync + 'static, F, Fetch> SourceSyncDriver<S, B, Ctx, F, Fetch> {
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
        // Capture the confirmed-watermark receiver before the engine is moved
        // behind the run-once mutex — it is the only handle the chain-head has
        // onto what the finalised store has committed.
        let confirmed_watermark = engine.subscribe_confirmed_watermark();
        Self {
            engine: Mutex::new(Some(engine)),
            provisioner,
            start,
            finalised_depth,
            channel_capacity,
            backend,
            confirmed_watermark,
            deferral: DeferralPolicy::default(),
            defer_threshold: DEFER_THRESHOLD_BLOCKS,
        }
    }

    /// Set the deferral policy for the initial catch-up (default
    /// [`DeferralPolicy::Auto`]).
    ///
    /// A consuming builder so a composition root threads the configured policy
    /// onto the driver it has just built — `resuming(..)?.with_deferral(policy)`
    /// — without widening the resume constructors, whose other callers keep the
    /// default.
    #[must_use]
    pub fn with_deferral(mut self, policy: DeferralPolicy) -> Self {
        self.deferral = policy;
        self
    }

    /// A receiver onto the engine's confirmed watermark — the highest height the
    /// finalised store has durably committed, or `None` on a fresh backend.
    ///
    /// The composed runtime hands this to the non-finalised chain-head so it can
    /// gate trimming on what the finalised store can already serve
    /// (confirm-before-trim), closing the seam between the two.
    pub fn subscribe_confirmed_watermark(&self) -> watch::Receiver<Option<Height>> {
        self.confirmed_watermark.clone()
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
        pipelines: IndexPipelines<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
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
        pipelines: IndexPipelines<Ctx>,
        source: Arc<S>,
        build: F,
        tuning: SyncTuning,
    ) -> Result<Self, IndexerError> {
        Self::assemble_resuming(backend, pipelines, source, build, tuning)
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
    ) -> Result<Self, IndexerError> {
        Self::assemble_resuming(backend, pipelines, source, build, tuning)
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
            let finalised_depth = self.finalised_depth;
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
                            // The indexer only builds the append-only finalised
                            // range, so its progress target is the finalised
                            // boundary (tip − reorg margin), mirroring `finalised`
                            // — not the live tip. Reporting the live tip would peg
                            // it at a chronic 99.9%, never reaching its own
                            // caught-up height; the chain-head (NFS) is what tracks
                            // the live tip.
                            let target = tip_source
                                .current_tip()
                                .await
                                .ok()
                                .map(|tip| u64::from(u32::from(tip.saturating_sub(finalised_depth))));
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
        let need_catchup = u32::from(synced) >= u32::from(self.start);
        let gap = u32::from(synced).saturating_sub(u32::from(self.start));

        // Bulk-mode bracket. A previous run that crashed mid-catch-up or
        // mid-finish leaves a namespace deferred; that must be completed on this
        // start whatever the policy or the remaining gap. Otherwise the policy
        // and the gap decide. `begin_bulk` is a no-op on a backend that does not
        // defer, so the non-LMDB path is unaffected. Errors here fail the
        // component loudly, typed — a half-entered bulk must not serve.
        let pending = self
            .backend
            .bulk_pending()
            .map_err(IndexerError::BulkProbe)?;
        let bulk = enters_bulk(self.deferral, pending, gap, self.defer_threshold);
        if bulk {
            self.backend
                .begin_bulk(BulkPolicy { enabled: true })
                .map_err(IndexerError::Bulk)?;
        }

        if need_catchup {
            self.sync_to(&mut engine, self.start, synced).await?;
        }

        // Finish the bulk load before reporting Ready: the deferred namespaces
        // read as `NotServiceable` until their run logs merge in, so the
        // component must not claim Ready while they are still incomplete. The
        // merge can run for minutes on mainnet — the backend logs its per-
        // namespace progress at `info`.
        if bulk {
            info!("finalising deferred indexes");
            self.backend.finish_bulk().map_err(IndexerError::Bulk)?;
        }
        reporter.ready();

        // Steady-state follow: index each new range as the tip advances. A
        // source that offers no tip subscription cannot be followed: the index
        // stays at the caught-up height for the life of the run, which is a
        // wiring fault (a composite synthesises the subscription by polling),
        // so it is said loudly rather than parked on quietly.
        let Some(mut tips) = self.provisioner.subscribe_tip() else {
            warn!(
                synced = u32::from(synced),
                "source offers no tip subscription; the finalised index will not advance past its \
                 caught-up height — wire a tip poller on the validator"
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
                    // Copy the height out before awaiting (drop the watch borrow),
                    // then cap at the finalised boundary — we only index append-only.
                    let tip = self.finalised(tips.borrow_and_update().height);
                    if tip > synced {
                        let from = synced
                            .checked_add(1)
                            .expect("tip below max height has a successor");
                        match self.sync_to(&mut engine, from, tip).await {
                            Ok(()) => synced = tip,
                            Err(error) => {
                                // The source could not serve part of the range —
                                // a state cache that has not caught up to the
                                // boundary, or a transport the fallback cannot
                                // reach. Whatever was fetched before the failure
                                // is committed and stamped; resume from there on
                                // the next tip change rather than take the
                                // runtime down for a range the next poll may
                                // serve. Not retried here: the source has already
                                // retried transient failures under its own policy.
                                synced = SyncEngine::<Ctx, B>::committed_height(&self.backend)?
                                    .and_then(|height| u32::try_from(height.value()).ok())
                                    .and_then(|height| Height::try_from(height).ok())
                                    .unwrap_or(synced);
                                warn!(
                                    %error,
                                    from = u32::from(from),
                                    to = u32::from(tip),
                                    resumed_at = u32::from(synced),
                                    "follow sync failed; the finalised index resumes from its \
                                     committed watermark on the next tip change"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod bulk_tests {
    //! The deferral trigger: the policy/gap/pending decision in isolation, and
    //! the bracket wired through a real catch-up with a backend that records its
    //! `begin_bulk`/`finish_bulk` calls.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use zaino_component::{ComponentName, Lifecycle, Managed, StatusWatch};
    use zaino_primitives::types::{Block, Height};
    use zaino_runtime::RunComponent;
    use zaino_source::mock::{test_block, MockChain};
    use zaino_source::{RetryPolicy, ValidatorClient};
    use zaino_sync::backend::{Backend, BulkPolicy, CommitError, FlushError, OpenError, ReadError};
    use zaino_sync::engine::{EngineConfig, SyncEngine};
    use zaino_sync::primitives::BlockHeight;
    use zaino_sync::testing::{toy_pipelines, InMemoryBackend, TestBlockContext};

    use super::{
        enters_bulk, DeferralPolicy, FetchConcurrency, FullBlocks, SourceProvisioner,
        SourceSyncDriver,
    };

    #[test]
    fn enters_bulk_requires_policy_and_threshold() {
        // Auto: enter only once the gap reaches the threshold.
        assert!(enters_bulk(DeferralPolicy::Auto, false, 50_000, 50_000));
        assert!(enters_bulk(DeferralPolicy::Auto, false, 50_001, 50_000));
        assert!(!enters_bulk(DeferralPolicy::Auto, false, 49_999, 50_000));
        // Off never enters on the threshold, whatever the gap.
        assert!(!enters_bulk(DeferralPolicy::Off, false, 1_000_000, 50_000));
    }

    #[test]
    fn a_pending_bulk_is_entered_regardless_of_policy_or_gap() {
        // A crash left a namespace deferred: complete it even below the threshold
        // and even under `off` — leaving it unreadable is never a policy.
        assert!(enters_bulk(DeferralPolicy::Auto, true, 0, 50_000));
        assert!(enters_bulk(DeferralPolicy::Off, true, 0, 50_000));
    }

    /// What the catch-up called on the backend.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum BulkCall {
        Begin,
        Finish,
    }

    /// An [`InMemoryBackend`] that records its bulk calls and reports a
    /// configurable pending state, so a test can assert the indexer's bracket
    /// without a durable store. Clones share the record and the inner store.
    #[derive(Clone)]
    struct RecordingBackend {
        inner: InMemoryBackend,
        calls: Arc<Mutex<Vec<BulkCall>>>,
        pending: Arc<AtomicBool>,
    }

    impl RecordingBackend {
        fn new(pending: bool) -> Self {
            Self {
                inner: InMemoryBackend::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
                pending: Arc::new(AtomicBool::new(pending)),
            }
        }

        fn calls(&self) -> Vec<BulkCall> {
            self.calls.lock().expect("calls mutex poisoned").clone()
        }
    }

    impl Backend for RecordingBackend {
        type Reader = <InMemoryBackend as Backend>::Reader;
        type Writer = <InMemoryBackend as Backend>::Writer;

        fn reader(&self) -> Result<Self::Reader, OpenError> {
            self.inner.reader()
        }

        fn writer(&self) -> Result<Self::Writer, OpenError> {
            self.inner.writer()
        }

        fn flush(&self) -> Result<(), FlushError> {
            self.inner.flush()
        }

        fn begin_bulk(&self, policy: BulkPolicy) -> Result<(), CommitError> {
            self.calls
                .lock()
                .expect("calls mutex poisoned")
                .push(BulkCall::Begin);
            self.inner.begin_bulk(policy)
        }

        fn finish_bulk(&self) -> Result<(), CommitError> {
            self.calls
                .lock()
                .expect("calls mutex poisoned")
                .push(BulkCall::Finish);
            self.inner.finish_bulk()
        }

        fn bulk_pending(&self) -> Result<bool, ReadError> {
            Ok(self.pending.load(Ordering::SeqCst))
        }
    }

    /// Project a fetched block into the toy set's context (height only).
    fn to_context(block: Block) -> TestBlockContext {
        TestBlockContext {
            height: u64::from(block.header.height),
            value: u32::from(block.header.height),
        }
    }

    /// Drive a catch-up over `blocks` heights (0..=blocks) to Ready, with the
    /// given policy, threshold and backend pending state, and return the bulk
    /// calls the backend recorded. `begin_bulk`/`finish_bulk` are no-ops on the
    /// in-memory store, so this exercises the indexer's decision and ordering,
    /// not the LMDB mechanics (those are the backend's own tests).
    async fn record_catchup(
        deferral: DeferralPolicy,
        threshold: u32,
        pending: bool,
        blocks: u32,
    ) -> Vec<BulkCall> {
        let mut chain = MockChain::new();
        for h in 0..=blocks {
            chain = chain.with_block(test_block(h, u8::try_from(h % 256).expect("byte")));
        }

        let backend = RecordingBackend::new(pending);
        let engine = SyncEngine::from_pipelines(
            toy_pipelines(),
            backend.clone(),
            EngineConfig {
                batch_size: 4,
                start_height: BlockHeight::new(0),
            },
        )
        .expect("valid index set");

        let source = ValidatorClient::new(chain, RetryPolicy::default());
        let provisioner = Arc::new(SourceProvisioner::<_, _, _, FullBlocks>::new(
            Arc::new(source),
            to_context,
            FetchConcurrency::SERIAL,
        ));
        let mut driver = SourceSyncDriver::new(
            engine,
            provisioner,
            Height::try_from(0).expect("valid height"),
            0, // finalised_depth: non-reorging mock, index right to the tip
            16,
            backend.clone(),
        )
        .with_deferral(deferral);
        driver.defer_threshold = threshold;

        let indexer = RunComponent::new(ComponentName("indexer"), driver);
        indexer.spawn().await.expect("spawn");

        let mut status = indexer.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if status.borrow_and_update().lifecycle == Lifecycle::Ready {
                    return;
                }
                status.changed().await.expect("status stream open");
            }
        })
        .await
        .expect("indexer reached Ready");

        // Ready is reported only after `finish_bulk` returns, so whatever the
        // bracket called is already recorded by the time we observe Ready.
        let calls = backend.calls();
        indexer.stop().await.expect("stop");
        calls
    }

    #[tokio::test]
    async fn enabled_above_threshold_brackets_catch_up_once_before_ready() {
        // Gap of 7 (heights 0..=7) over a threshold of 2: bulk mode is entered
        // before the catch-up and finished before Ready, each exactly once.
        let calls = record_catchup(DeferralPolicy::Auto, 2, false, 7).await;
        assert_eq!(calls, vec![BulkCall::Begin, BulkCall::Finish]);
    }

    #[tokio::test]
    async fn off_never_enters_bulk() {
        let calls = record_catchup(DeferralPolicy::Off, 2, false, 7).await;
        assert!(
            calls.is_empty(),
            "off must reproduce the direct path: {calls:?}"
        );
    }

    #[tokio::test]
    async fn below_threshold_does_not_enter_bulk() {
        // Gap of 7 under a threshold of 1000: too small to defer.
        let calls = record_catchup(DeferralPolicy::Auto, 1000, false, 7).await;
        assert!(
            calls.is_empty(),
            "a small gap stays on the direct path: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_pending_bulk_is_completed_even_below_threshold() {
        // The backend reports a bulk left pending by a crash; the indexer
        // re-enters and finishes it though the gap is far below the threshold,
        // and even under `off`.
        let calls = record_catchup(DeferralPolicy::Off, 1000, true, 7).await;
        assert_eq!(calls, vec![BulkCall::Begin, BulkCall::Finish]);
    }
}
