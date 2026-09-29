//! Boots the Zaino daemon and composes its pipeline.
//!
//! One fetch, N indexes, one server. The stack crates stay config-agnostic; this module is the
//! only place daemon config crosses into them, and the only place the pipeline's shape is
//! written down (`docs/design/sync.md`):
//!
//! ```text
//!   validators ──▶ ChainView ── quorum tip ──▶ Producer ──▶ BlockSink ─┬─▶ IndexFollower(compact_block) ◀┐ Zip
//!   validators ──▶ BlockFetchPool ───────────────┘                     ├─▶ IndexFollower(value_balance)  │ (lockstep)
//!                                                                      │     └─▶ FeeSink ────────────────┘
//!                                                                      ├─▶ IndexFollower(block_hash)
//!                                                                      ├─▶ IndexFollower(tree_state)
//!                                                                      ├─▶ IndexFollower(transparent_address)
//!                                                                      └─▶ (further indexes subscribe here)
//!
//!   non-finalized + files ──▶ CompactBlockService       ──┐
//!   non-finalized + files ──▶ BlockHashService          ──┤ (by-hash locator for the other two)
//!   non-finalized + files ──▶ TreeStateService          ──┼─▶ Router
//!   non-finalized + runs  ──▶ TransparentAddressService ──┘
//! ```
//!
//! - Stage → stage = a channel, wired here by hand (no scheduler, no dependency graph)
//! - Every `IndexFollower` also reads the quorum tip (serving gate, bulk / follow switch); the
//!   sink carries blocks only
//! - Every stage = one plain task in a `JoinSet`; fallible setup awaited before any spawn
//! - Scope: compact-block, block-hash, tree-state, transparent-address slices from their indexes,
//!   plus `SendTransaction`/`GetLightdInfo` off the validator

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _, Span};

use zaino_chainview::QuorumTip;
use zaino_grpc::{GrpcLimits, GrpcServer, TrustedProxies, ValidatorHandler};
use zaino_index_compact_block::{CompactBlockIndexWriter, CompactBlockService, CompactBlockStore};
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressService};
use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateService, TreeStateStore};
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashService, BlockHashStore};
use zaino_internal_value_balance::ValueBalanceIndexWriter;
use zaino_persistence::fs::{Fs, RealFs};
use zaino_primitives::types::{Block, ReorgDepth};
use zaino_source::{BlockFetchPool, FetchRoute, ZebraRpcAdapter};
use zaino_sync::{BlockSink, FeeSink, IndexFollower, IndexWriter, Producer, Zip};
use zcash_protocol::consensus::NetworkType;

use crate::config::{DaemonConfig, SourceConfig, ZainoIndexConfig};
use crate::error::IndexerError;
use crate::index_report::Watched;

/// Task name + outcome (name → log line for a task that ends early with `Ok`)
type TaskExit = (&'static str, Result<(), IndexerError>);

/// Start the Zaino daemon.
///
/// Returns a handle that resolves when the runtime exits: `Ok(())` on a shutdown signal, or the
/// first task's failure (the process then exits; nothing restarts in-process).
pub async fn start_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    warn!("In development, not for production mainnet use");
    spawn_indexer(config).await
}

/// Wait for the validator's JSON-RPC to answer, build the source over it, then boot the runtime.
pub async fn spawn_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.validate()?;
    crate::fd_limit::raise_for(config.grpc.max_connections)?;
    let validator = Arc::new(
        connect_validator(&config.source)
            .instrument(crate::logging::component("ChainView"))
            .await?,
    );
    boot(validator, config).await
}

async fn connect_validator(source: &SourceConfig) -> Result<ZebraRpcAdapter, IndexerError> {
    let adapter = ZebraRpcAdapter::connect(
        &source.jsonrpc_address,
        source.cookie_path.as_deref(),
        source.user.clone(),
        source.password.clone(),
    )
    .await?;
    info!(endpoint = %source.jsonrpc_address, "Validator reachable");
    Ok(adapter)
}

/// Compose the pipeline over the shared `validator`, then spawn every stage.
///
/// - One `Arc<ZebraRpcAdapter>` per validator, shared by the fetch pool, chainview and the gRPC
///   fallback
async fn boot(
    validator: Arc<ZebraRpcAdapter>,
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    // --- the chain view: quorum tip, mempool and broadcast fan-out, over every validator
    let chainview_span = crate::logging::component("ChainView");
    let chainview = crate::chainview::connect(Arc::clone(&validator), &config)
        .instrument(chainview_span.clone())
        .await?;
    let tips = chainview.handles.view.subscribe_tip();

    // --- the indexes: each its own files, its own finalised height, its own sink subscription
    //
    // `enabled = false` → store unopened, unsubscribed, no task, routes unclaimed (validator
    // fallback). compact-block not optional (`DaemonConfig::validate`)
    let depth = ReorgDepth::new(config.fetch.finalised_depth);
    let mut block_sink = BlockSink::new("blocks");
    let fs = RealFs::shared();

    // compact-block reads its blocks and value-balance's republished fees in lockstep: both
    // subscribed before value-balance's follower takes the fee sink
    let mut fee_sink = FeeSink::new("fees");
    let (index, fees) = (&config.index.compact_block, &config.index.value_balance);
    let (compact_block_span, store) = open_index(CompactBlockIndexWriter::NAME, index, || {
        Ok(CompactBlockStore::open(Arc::clone(&fs), &index.path, config.network)?)
    })?;
    let feed = Zip::new(
        block_sink.subscribe(CompactBlockIndexWriter::NAME, index.queue_bytes()),
        fee_sink.subscribe(CompactBlockIndexWriter::NAME, fees.queue_bytes()),
    );
    let writer = CompactBlockIndexWriter::new(store);
    let compact_block = IndexFollower::new(writer, feed, tips.clone(), index.batch_bytes(), depth);
    let (value_balance_span, writer) = open_index(ValueBalanceIndexWriter::NAME, fees, || {
        Ok(ValueBalanceIndexWriter::open(Arc::clone(&fs), &fees.path, config.network)?)
    })?;
    let sink = &mut block_sink;
    let value_balance = follow(sink, writer, fees, &tips, depth).publishing(fee_sink);
    let (index, network) = (&config.index, config.network);
    let block_hash = open_block_hash(&fs, &index.block_hash, network)?
        .map(|(span, writer)| (span, follow(sink, writer, &index.block_hash, &tips, depth)));
    let tree_state = open_tree_state(&fs, &index.tree_state, network)?
        .map(|(span, writer)| (span, follow(sink, writer, &index.tree_state, &tips, depth)));
    let transparent = open_transparent_address(&fs, &index.transparent_address, network)?.map(
        |(span, writer)| (span, follow(sink, writer, &index.transparent_address, &tips, depth)),
    );
    // every subscriber's durable tip (production starts after the rearmost)
    let durable = [
        Some(compact_block.writer().finalized_height()),
        Some(value_balance.writer().finalized_height()),
        block_hash.as_ref().map(|(_, follower)| follower.writer().finalized_height()),
        tree_state.as_ref().map(|(_, follower)| follower.writer().finalized_height()),
        transparent.as_ref().map(|(_, follower)| follower.writer().finalized_height()),
    ];

    let compact_block_service = CompactBlockService::new(compact_block.served())
        .with_max_range(config.serve.max_block_range);
    let block_hash_service =
        block_hash.as_ref().map(|(_, follower)| BlockHashService::new(follower.served()));
    let tree_state_service = tree_state
        .as_ref()
        .map(|(_, follower)| TreeStateService::new(follower.served(), config.network));
    let transparent_service = transparent.as_ref().map(|(_, follower)| {
        TransparentAddressService::new(follower.served(), config.network)
            .with_max_rows(config.serve.max_address_rows)
    });

    // --- the producer: bulk over every validator (or the primary), then chainview's quorum tip
    let pool = BlockFetchPool::new(
        chainview.sources.clone(),
        config.primary_validator_index().map_or(FetchRoute::Spread, FetchRoute::Primary),
        config.fetch.concurrency,
    );
    let producer = Producer::new(block_sink, pool, tips, depth, durable.into_iter().flatten())
        .with_live_span(crate::logging::component("ZainoNFS"));

    // --- serving: index first, validator behind it; bound here (EADDRINUSE = boot failure)
    let mut server = GrpcServer::new(
        ValidatorHandler::new(
            Arc::clone(&validator),
            compact_block_service.clone(),
            config.network,
        ),
        config.serve.grpc_listen_address,
        GrpcLimits::from(&config.grpc),
    )
    .with_trusted_proxies(TrustedProxies::new(config.grpc.trusted_proxies.clone()))
    .with_compact_block(compact_block_service)
    .with_chainview(chainview.handles.clone());
    if let Some(service) = block_hash_service {
        server = server.with_block_hash(service);
    }
    if let Some(service) = tree_state_service {
        server = server.with_tree_state(service);
    }
    if let Some(service) = transparent_service {
        server = server.with_transparent_address(service);
    }
    let server = server.bind().await?;
    let grpc_span = crate::logging::component("Grpc");
    grpc_span.in_scope(|| {
        info!(
            endpoint = %config.serve.grpc_listen_address,
            network = crate::config::network_name(config.network),
            "Listening"
        )
    });

    crate::metrics::track_index(&compact_block);
    crate::metrics::track_index(&value_balance);
    if let Some((_, follower)) = &block_hash {
        crate::metrics::track_index(follower);
    }
    if let Some((_, follower)) = &tree_state {
        crate::metrics::track_index(follower);
    }
    if let Some((_, follower)) = &transparent {
        crate::metrics::track_index(follower);
    }

    // --- run: nothing fallible left, every stage one task
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let mut report = |span: &Span, watched, config: &ZainoIndexConfig| {
        let run = crate::index_report::run(watched, config.path.clone(), cancel.child_token());
        spawn(&mut tasks, "index-report", span.clone(), run);
    };
    let index = &config.index;
    report(&compact_block_span, gates(&compact_block), &index.compact_block);
    // no service reads value-balance (compact-block takes its fees through the sink)
    let unread = Watched { reads: None, ..gates(&value_balance) };
    report(&value_balance_span, unread, &index.value_balance);
    if let Some((span, follower)) = &block_hash {
        report(span, gates(follower), &index.block_hash);
    }
    if let Some((span, follower)) = &tree_state {
        report(span, gates(follower), &index.tree_state);
    }
    if let Some((span, follower)) = &transparent {
        report(span, gates(follower), &index.transparent_address);
    }
    // followers: the root token (a failure cancels everything), stopped by the producer's Shutdown
    spawn(&mut tasks, "compact-block", compact_block_span, compact_block.run(cancel.clone()));
    spawn(&mut tasks, "value-balance", value_balance_span, value_balance.run(cancel.clone()));
    if let Some((span, follower)) = block_hash {
        spawn(&mut tasks, "block-hash", span, follower.run(cancel.clone()));
    }
    if let Some((span, follower)) = tree_state {
        spawn(&mut tasks, "tree-state", span, follower.run(cancel.clone()));
    }
    if let Some((span, follower)) = transparent {
        spawn(&mut tasks, "transparent-address", span, follower.run(cancel.clone()));
    }
    for poller in chainview.pollers {
        spawn(&mut tasks, "chainview", chainview_span.clone(), poller.run(cancel.child_token()));
    }
    let source_span = crate::logging::component("ZainoSource");
    spawn(&mut tasks, "producer", source_span, producer.run(cancel.child_token()));
    spawn(&mut tasks, "grpc", grpc_span, server.run(cancel.child_token()));
    spawn(
        &mut tasks,
        "heartbeat",
        crate::logging::component("Metrics"),
        crate::admin::beat(cancel.child_token()),
    );

    Ok(tokio::spawn(supervise(tasks, cancel)))
}

/// Signal → `Ok(())`; else the first failure (ending cleanly before shutdown is one)
///
/// - Either way: cancel the rest, then wait for them (followers flush what is final)
/// - Failed follower = cancel, its error joined once the producer's `Shutdown` reaches it
async fn supervise(
    mut tasks: JoinSet<TaskExit>,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    let mut failure = tokio::select! {
        signal = shutdown_signal() => {
            info!(signal, "Shutdown signal received");
            None
        }
        () = cancel.cancelled() => None,
        Some(exit) = tasks.join_next() => Some(first_failure(exit)),
    };
    cancel.cancel();
    while let Some(exit) = tasks.join_next().await {
        match exit {
            Ok((task, Ok(()))) => debug!(task, "Task stopped"),
            Ok((task, Err(error))) => {
                error!(task, %error, "Task failed");
                failure.get_or_insert(error);
            }
            Err(error) => {
                error!(%error, "Task panicked");
                failure.get_or_insert(IndexerError::TokioJoinError(error));
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

/// A task ended before any shutdown signal: always a fault
fn first_failure(exit: Result<TaskExit, tokio::task::JoinError>) -> IndexerError {
    match exit {
        Ok((task, Ok(()))) => IndexerError::TaskEnded { task },
        Ok((task, Err(error))) => {
            error!(task, %error, "Task failed");
            error
        }
        Err(error) => IndexerError::TokioJoinError(error),
    }
}

fn spawn<E>(
    tasks: &mut JoinSet<TaskExit>,
    task: &'static str,
    component: Span,
    run: impl Future<Output = Result<(), E>> + Send + 'static,
) where
    IndexerError: From<E>,
{
    tasks.spawn(async move { (task, run.await.map_err(IndexerError::from)) }.instrument(component));
}

/// `open` under the index's component span, logged; the span then carries the index's task
fn open_index<W>(
    name: &str,
    config: &ZainoIndexConfig,
    open: impl FnOnce() -> Result<W, IndexerError>,
) -> Result<(Span, W), IndexerError> {
    let span = crate::logging::index_component(name);
    let opened = span.in_scope(|| {
        info!("Opening from {}", crate::logging::shown_path(&config.path));
        open()
    })?;
    Ok((span, opened))
}

/// Enabled → an index writer over its files; disabled → `None`, and `config.path` is not created.
fn open_block_hash(
    fs: &Arc<dyn Fs>,
    config: &ZainoIndexConfig,
    network: NetworkType,
) -> Result<Option<(Span, BlockHashIndexWriter)>, IndexerError> {
    let open = || {
        let store = BlockHashStore::open(Arc::clone(fs), &config.path, network)?;
        Ok(BlockHashIndexWriter::new(store))
    };
    config.enabled.then(|| open_index(BlockHashIndexWriter::NAME, config, open)).transpose()
}

/// Enabled → an index writer over its files; disabled → `None`, and `config.path` is not created.
fn open_tree_state(
    fs: &Arc<dyn Fs>,
    config: &ZainoIndexConfig,
    network: NetworkType,
) -> Result<Option<(Span, TreeStateIndexWriter)>, IndexerError> {
    let open = || {
        let store = TreeStateStore::open(Arc::clone(fs), &config.path, network)?;
        Ok(TreeStateIndexWriter::new(store)?)
    };
    config.enabled.then(|| open_index(TreeStateIndexWriter::NAME, config, open)).transpose()
}

/// Enabled → an index writer over its files; disabled → `None`, and `config.path` is not created.
fn open_transparent_address(
    fs: &Arc<dyn Fs>,
    config: &ZainoIndexConfig,
    network: NetworkType,
) -> Result<Option<(Span, TransparentAddressIndexWriter)>, IndexerError> {
    let open = || {
        let fs = Arc::clone(fs);
        Ok(TransparentAddressIndexWriter::open(fs, &config.path, network)?)
    };
    config
        .enabled
        .then(|| open_index(TransparentAddressIndexWriter::NAME, config, open))
        .transpose()
}

/// `follower`'s tips, serving gate and request count, for its status report
fn gates<W: IndexWriter, F, D>(follower: &IndexFollower<W, F, D>) -> Watched {
    Watched {
        finalized: follower.subscribe_finalized(),
        applied: follower.subscribe_applied(),
        synced: follower.subscribe_synced(),
        reads: Some(follower.reads()),
    }
}

/// `writer`'s own queue off `block_sink`, committing per `config.batch_mib`
fn follow<W: IndexWriter<Input = Block>>(
    block_sink: &mut BlockSink,
    writer: W,
    config: &ZainoIndexConfig,
    tips: &watch::Receiver<Option<QuorumTip>>,
    depth: ReorgDepth,
) -> IndexFollower<W> {
    let subscription = block_sink.subscribe(W::NAME, config.queue_bytes());
    IndexFollower::new(writer, subscription, tips.clone(), config.batch_bytes(), depth)
}

/// Wait for a process shutdown signal, returning which one arrived.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Registering a signal handler only fails on a broken runtime/OS, which
        // is an unrecoverable process-level invariant, not a runtime condition.
        let mut terminate = signal(SignalKind::terminate()).expect("register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Any task ending first is the daemon's failure, named for what it was (clean end, its own
    /// error, a panic), returned only after every other task saw the cancel and finished
    #[tokio::test]
    async fn a_task_ending_first_cancels_and_drains_the_rest_then_fails_the_daemon() {
        type Run = Pin<Box<dyn Future<Output = Result<(), IndexerError>> + Send>>;
        let early: [(&str, Run); 3] = [
            ("ends ok", Box::pin(async { Ok(()) })),
            ("fails", Box::pin(async { Err(IndexerError::ConfigError("boom".into())) })),
            ("panics", Box::pin(async { panic!("boom") })),
        ];

        for (case, run) in early {
            let cancel = CancellationToken::new();
            let drained = Arc::new(AtomicBool::new(false));
            let mut tasks = JoinSet::new();
            let (token, flag) = (cancel.child_token(), Arc::clone(&drained));
            spawn(&mut tasks, "waits-for-cancel", Span::none(), async move {
                token.cancelled().await;
                tokio::task::yield_now().await;
                flag.store(true, Ordering::SeqCst);
                Ok::<_, IndexerError>(())
            });
            spawn(&mut tasks, "early", Span::none(), run);

            let outcome = supervise(tasks, cancel.clone()).await;

            let named = match case {
                "ends ok" => matches!(outcome, Err(IndexerError::TaskEnded { task: "early" })),
                "fails" => {
                    matches!(outcome, Err(IndexerError::ConfigError(ref boom)) if boom == "boom")
                }
                _ => {
                    matches!(outcome, Err(IndexerError::TokioJoinError(ref join)) if join.is_panic())
                }
            };
            assert!(named, "{case}: {outcome:?}");
            assert!(cancel.is_cancelled(), "{case}: rest not cancelled");
            assert!(drained.load(Ordering::SeqCst), "{case}: returned before the drain");
        }
    }

    /// `enabled = false` is honoured before anything touches the disk: no index writer, and the
    /// index's directory is never created — so nothing can subscribe or claim a route either.
    #[test]
    fn a_disabled_index_is_never_constructed_and_creates_no_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = |name: &str, enabled: bool| ZainoIndexConfig {
            enabled,
            path: dir.path().join(name),
            batch_mib: NonZeroU32::MIN,
            queue_mib: NonZeroU32::MIN,
        };

        let (fs, net) = (RealFs::shared(), NetworkType::Regtest);

        let off = index("block-hash-off", false);
        assert!(open_block_hash(&fs, &off, net).expect("disabled").is_none());
        assert!(!off.path.exists(), "{}", off.path.display());

        let off = index("tree-state-off", false);
        assert!(open_tree_state(&fs, &off, net).expect("disabled").is_none());
        assert!(!off.path.exists(), "{}", off.path.display());

        let off = index("transparent-off", false);
        assert!(open_transparent_address(&fs, &off, net).expect("disabled").is_none());
        assert!(!off.path.exists(), "{}", off.path.display());

        let on = index("block-hash-on", true);
        assert!(open_block_hash(&fs, &on, net).expect("enabled").is_some());
        assert!(on.path.exists(), "{}", on.path.display());

        let on = index("tree-state-on", true);
        assert!(open_tree_state(&fs, &on, net).expect("enabled").is_some());
        assert!(on.path.exists(), "{}", on.path.display());

        let on = index("transparent-on", true);
        assert!(open_transparent_address(&fs, &on, net).expect("enabled").is_some());
        assert!(on.path.exists(), "{}", on.path.display());
    }
}
