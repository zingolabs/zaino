//! Runnable demonstration of the runtime's structural error handling + logging.
//!
//! Boots a *real* Orchestra (an observed validator + an owned indexer) over
//! **mock sources rigged to fail hard**, and shows a single failure crossing the
//! whole machinery, structurally:
//!
//!   * logged with its full **typed source chain** at the one supervision
//!     boundary (`zaino_runtime::indexer`), never stringified, never swallowed;
//!   * recorded on the component's [`ComponentStatus::reason`], so a health
//!     reader sees *why* it is `Critical`, not merely *that* it is;
//!   * **escalated** by name up the runtime's shared channel — while the
//!     observed validator stays `Healthy`.
//!
//! Four failure classes are shown:
//!
//!   1. **Typed transport failure** — the source hands back something the
//!      indexer cannot use. Triggered here by a mock whose `get_block` returns a
//!      non-retryable `NonDomain(Parse)` error; it bubbles as
//!      `IndexerError::Transport` — a failure handled as *data*, not a crash.
//!   2. **Panic in a joined sub-task (the block-fetch pump)** — a bug deep in a
//!      block fetch (e.g. a failed assertion or `.expect`). Triggered here by a
//!      mock source whose `get_block` panics; the provisioner runs that fetch on
//!      a spawned `Task`, so the panic hook logs it at **origin**
//!      (`target: "panic"`) and it resurfaces at the boundary — via
//!      `Task::join` — as a named `IndexerError::UnexpectedWorkerFailure` (task
//!      "block-provisioner").
//!   3. **Panic in the run loop body itself (the tip fetch)** — a panic *outside*
//!      any joined sub-task. Triggered here by a mock whose `get_chain_tip`
//!      panics (the tip fetch is awaited directly in `run`, not on the pump).
//!      This was a **silent death** before the supervised-run fix (status frozen
//!      at `Syncing`, no escalation); now the run boundary wraps the loop in
//!      `zaino_async::catch_panic`, turning it into `Critical` + escalation like
//!      any other failure.
//!   4. **A bubbled multi-layer error** — a driver returning `IndexerError::Domain`
//!      wrapping a three-level decode chain, so the boundary renders the *full*
//!      source chain via `error_chain` (`outer: middle: root`) on the log's
//!      `cause` field and the component's `reason` — versus the terminal leaves of
//!      (1)–(3), whose `error` and `cause` coincide.
//!
//! Run it and watch stderr:
//!
//! ```text
//! cargo run -p zaino-runtime --example error_machinery
//! ZAINOLOG_FORMAT=json cargo run -p zaino-runtime --example error_machinery
//! RUST_LOG=zaino=debug,panic=error cargo run -p zaino-runtime --example error_machinery
//! ```
//!
//! [`ComponentStatus::reason`]: zaino_component::ComponentStatus::reason

use std::sync::Arc;

use zaino_component::{
    CancellationToken, ComponentName, ComponentStatus, ReachabilityProbe, ReadySignal,
};
use zaino_indexer::{FetchConcurrency, IndexerError, SourceSyncDriver, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{Block, BlockHash, Height};
use zaino_runtime::{IndexerComponent, OrchestraBuilder, SyncDriver, ValidatorComponent};
use zaino_source::{
    FailureMode, GetBlockError, GetChainTipError, NonDomainError, OneShotGetBlock,
    OneShotGetChainTip, QueryError, RetryPolicy, SubscribeChainTip, ValidatorClient,
    ValidatorSource,
};

/// A validator reachability probe that always answers `reachable`.
struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

/// The tip a rigged source reports, so the driver has a non-empty range to sync
/// and actually reaches the (failing) block fetch. Height 2 with `finalised_depth
/// = 0` gives a sync range of `[0, 2]`; the first fetch is where the failure lands.
fn rigged_tip() -> (BlockHash, Height) {
    (
        BlockHash::from([1u8; 32]),
        Height::try_from(2u32).expect("valid test height"),
    )
}

/// A source that serves a tip but returns a **non-retryable transport failure**
/// (`Parse`) on every block fetch — the "source handed back something
/// undecodable" case. The failure is a value, not a panic; it bubbles as a typed
/// `IndexerError::Transport`.
struct TransportFailSource;

impl ValidatorSource for TransportFailSource {
    type NonDomain = NonDomainError;
}

impl OneShotGetChainTip for TransportFailSource {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        Ok(rigged_tip())
    }
}

impl OneShotGetBlock for TransportFailSource {
    async fn get_block(&self, _height: Height) -> Result<Block, QueryError<GetBlockError>> {
        // Yield first, so the indexer is observed `Syncing` before it fails —
        // the failure lands *after* boot, on the escalate-while-running path.
        tokio::task::yield_now().await;
        Err(QueryError::NonDomain(NonDomainError::new(
            FailureMode::Parse,
            "source returned an undecodable block (a parse failure)",
        )))
    }
}

// No push subscription — the default `subscribe_to_chain_tip` returns `None`.
impl SubscribeChainTip for TransportFailSource {}

/// A source that serves a tip but **panics** in a block fetch — a bug deep in
/// the fetch path (a failed assertion / `.expect`). The provisioner runs the
/// fetch on a spawned `Task`, so tokio catches the panic; the panic hook logs it
/// at origin and `Task::join` surfaces it, named, as a `TaskError`.
struct PanicFetchSource;

impl ValidatorSource for PanicFetchSource {
    type NonDomain = NonDomainError;
}

impl OneShotGetChainTip for PanicFetchSource {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        Ok(rigged_tip())
    }
}

impl OneShotGetBlock for PanicFetchSource {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        tokio::task::yield_now().await;
        panic!(
            "simulated panic decoding the block at height {} (a failed `.expect` in the fetch path)",
            u32::from(height)
        );
    }
}

impl SubscribeChainTip for PanicFetchSource {}

/// A source that **panics in `get_chain_tip`** — a panic in the run loop *body*
/// (the tip fetch is awaited directly in `run`, not in the spawned pump). This is
/// the path that was a *silent death* before the supervised-run fix: the run task
/// would unwind, its handle abort, and the status stay frozen at `Syncing` with
/// no escalation. Now `zaino_async::catch_panic` at the run boundary turns it
/// into `Critical`.
struct PanicTipSource;

impl ValidatorSource for PanicTipSource {
    type NonDomain = NonDomainError;
}

impl OneShotGetChainTip for PanicTipSource {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        // Yield first, so the indexer is observed `Syncing` before it panics.
        tokio::task::yield_now().await;
        panic!("simulated panic in the tip fetch — a run-loop-body panic");
    }
}

impl OneShotGetBlock for PanicTipSource {
    async fn get_block(&self, _height: Height) -> Result<Block, QueryError<GetBlockError>> {
        unreachable!("get_chain_tip panics before any block is fetched")
    }
}

impl SubscribeChainTip for PanicTipSource {}

/// Boot a validator + indexer (over `source`), wait for the indexer's failure to
/// escalate through the runtime, and report what each layer saw.
async fn run_scenario<S>(source: S)
where
    S: OneShotGetBlock + OneShotGetChainTip + SubscribeChainTip + Send + Sync + 'static,
{
    let backend = InMemoryBackend::new();
    let source = Arc::new(ValidatorClient::new(source, RetryPolicy::default()));

    let driver = SourceSyncDriver::resuming(
        &backend,
        index_set(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            // Non-reorging test source: index right up to the tip.
            finalised_depth: 0,
            channel_capacity: 16,
            // Serial + deterministic: one fetch in flight, so the first failure
            // is the first fetch.
            concurrency: FetchConcurrency::SERIAL,
        },
    )
    .expect("driver builds");

    boot_and_await_failure(driver).await;
}

/// Boot a validator + an indexer driven by `driver`, wait for the indexer's
/// failure to escalate through the runtime, and report what each layer saw.
async fn boot_and_await_failure<D: SyncDriver>(driver: D) {
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    let mut orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .expect("indexer boots (reaches Syncing before it fails)")
        .build();

    println!("\n  ── waiting for the failure to escalate through the runtime ──\n");

    // The runtime's shared escalation channel: which component went Critical.
    let escalated = orchestra.next_escalation().await;
    println!(
        "\n  runtime escalation channel fired for: {:?}",
        escalated.map(|n| n.0)
    );

    println!("\n  component statuses the health machinery now reports:");
    dump(&orchestra.statuses());

    orchestra.shutdown();
}

/// A worker error with a genuine three-layer `#[source]` chain, so the boundary
/// has something real to render with `error_chain` — unlike the terminal leaves
/// of the other scenarios. Each level's `Display` is self-contained (the
/// thiserror norm), so the chain reads cleanly as `outer: middle: root`.
#[derive(Debug, thiserror::Error)]
#[error("decoding the compact block failed")]
struct BlockDecodeError(#[source] FieldDecodeError);

#[derive(Debug, thiserror::Error)]
#[error("field 'outputs' has an invalid CompactSize length prefix")]
struct FieldDecodeError;

/// A driver whose run loop returns a genuinely *bubbled* error — an
/// `IndexerError::Domain` wrapping the nested decode chain above — to show the
/// boundary logging and recording the **full source chain**, not just the
/// outermost variant.
struct NestedFailureDriver;

impl SyncDriver for NestedFailureDriver {
    type Error = IndexerError;

    async fn run(
        self: Arc<Self>,
        _cancel: CancellationToken,
        _caught_up: ReadySignal,
    ) -> Result<(), IndexerError> {
        // Yield so the indexer is observed `Syncing` before it fails.
        tokio::task::yield_now().await;
        Err(IndexerError::Domain(Box::new(BlockDecodeError(
            FieldDecodeError,
        ))))
    }
}

/// Print each component's status the way a health endpoint would read it —
/// crucially the `reason` on the failed one.
fn dump(statuses: &[ComponentStatus]) {
    for s in statuses {
        println!(
            "    {:<10} lifecycle={:<8} health={:?}",
            s.name.0,
            format!("{:?}", s.lifecycle),
            s.health,
        );
        if let Some(reason) = &s.reason {
            println!("               └─ reason: {reason}");
        }
    }
}

fn banner(title: &str, subtitle: &str) {
    println!("\n{}", "═".repeat(78));
    println!("  {title} — {subtitle}");
    println!("{}", "═".repeat(78));
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // The real logging machinery: structured tracing subscriber + the panic
    // hook that routes every panic through `tracing` at its origin.
    zaino_logging::try_init();

    banner(
        "SCENARIO 1",
        "typed transport failure (non-retryable source Parse error)",
    );
    println!("\n  Expect: a boundary error log carrying the typed source chain,");
    println!("  `IndexerError::Transport` → `NonDomainError`, and the indexer");
    println!("  Critical with that chain on its `reason`.");
    run_scenario(TransportFailSource).await;

    banner(
        "SCENARIO 2",
        "panic inside a spawned fetch task (a bug in the block-fetch path)",
    );
    println!("\n  Expect: the panic hook logging at origin (target=\"panic\", on the");
    println!("  worker thread), THEN the same failure resurfacing at the boundary");
    println!("  as a named `IndexerError::UnexpectedWorkerFailure` — a crashed fetch became a");
    println!("  logged, escalated component failure, not a silent death.");
    run_scenario(PanicFetchSource).await;

    banner(
        "SCENARIO 3",
        "panic in the run loop body itself (tip fetch) — the former silent death",
    );
    println!("\n  Expect: BEFORE the supervised-run fix this froze the indexer at");
    println!("  Syncing/Healthy with no escalation (a panic outside any joined");
    println!("  sub-task). Now the run-loop panic is caught, logged, and escalated");
    println!("  like any other failure.");
    run_scenario(PanicTipSource).await;

    banner(
        "SCENARIO 4",
        "a bubbled error — the full source chain, not just the outer variant",
    );
    println!("\n  Expect: a genuinely multi-layer error (`IndexerError::Domain`");
    println!("  wrapping a decode chain). The boundary renders the whole chain via");
    println!("  `error_chain` — `outer: middle: root` — on both the log's `cause`");
    println!("  field and the component's `reason`. (The other scenarios' errors are");
    println!("  terminal leaves, so their `error` and `cause` coincide.)");
    boot_and_await_failure(NestedFailureDriver).await;

    println!("\n{}", "═".repeat(78));
    println!("  done — every hard failure was logged, reasoned, and escalated.");
    println!("{}\n", "═".repeat(78));
}
