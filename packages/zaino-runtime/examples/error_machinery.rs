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
//! Two failure classes are shown:
//!
//!   1. **Typed transport failure** — the source returns a non-retryable
//!      `Parse` failure. Bubbles as `IndexerError::Transport`. This is the
//!      "the source handed back something undecodable" path, handled as data.
//!   2. **Panic deep in a block fetch** — the exact *class* of the Ironwood
//!      `.expect` blowup, without needing Ironwood. Shows the panic hook logging
//!      the panic at its **origin** (on the worker thread, `target: "panic"`),
//!      AND the panic resurfacing at the boundary as a named
//!      `IndexerError::UnexpectedWorkerFailure` (task "block-provisioner") — a crashed
//!      fetch task becomes a logged, escalated component failure rather than a
//!      silent death.
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

use zaino_component::{ComponentName, ComponentStatus, ReachabilityProbe};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{Block, BlockHash, Height};
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ValidatorComponent};
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
            "simulated block deserialize failure (Ironwood-class, as a typed error)",
        )))
    }
}

// No push subscription — the default `subscribe_to_chain_tip` returns `None`.
impl SubscribeChainTip for TransportFailSource {}

/// A source that serves a tip but **panics** deep in a block fetch — the exact
/// shape of the Ironwood `.expect` blowup. tokio catches the panic; the panic
/// hook logs it at origin and `Task::join` surfaces it named as a `TaskError`.
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
            "simulated deserialize panic at height {} — the Ironwood-class `.expect`",
            u32::from(height)
        );
    }
}

impl SubscribeChainTip for PanicFetchSource {}

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
        "panic deep in a block fetch (the Ironwood `.expect` class)",
    );
    println!("\n  Expect: the panic hook logging at origin (target=\"panic\", on the");
    println!("  worker thread), THEN the same failure resurfacing at the boundary");
    println!("  as a named `IndexerError::UnexpectedWorkerFailure` — a crashed fetch became a");
    println!("  logged, escalated component failure, not a silent death.");
    run_scenario(PanicFetchSource).await;

    println!("\n{}", "═".repeat(78));
    println!("  done — both hard failures were logged, reasoned, and escalated.");
    println!("{}\n", "═".repeat(78));
}
