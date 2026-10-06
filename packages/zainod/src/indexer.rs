//! Boots the Zaino daemon and composes its pipeline.
//!
//! One fetch, N indexes, one server. The stack crates stay config-agnostic; this module is the
//! only place daemon config crosses into them, and the only place the pipeline's shape is
//! written down (`docs/design/sync.md`):
//!
//! ```text
//!   validators ──▶ ChainView ── quorum tip ──▶ Producer ──▶ BlockSink ─┬─▶ compact_block.run ◀────┐ fees
//!   validators ──▶ BlockFetchPool ───────────────┘                     ├─▶ value_balance.run      │
//!                                                                      │     └─▶ FeeSink ─────────┘
//!                                                                      ├─▶ block_hash.run
//!                                                                      ├─▶ tree_state.run
//!                                                                      ├─▶ transparent_address.run
//!                                                                      └─▶ (further indexes subscribe here)
//!
//!   non-finalized + files ──▶ CompactBlockService       ──┐
//!   non-finalized + files ──▶ BlockHashService          ──┤ (by-hash locator for the other two)
//!   non-finalized + files ──▶ TreeStateService          ──┼─▶ Router
//!   non-finalized + runs  ──▶ TransparentAddressService ──┘
//! ```
//!
//! - Stage → stage = a channel, wired here by hand (no scheduler, no dependency graph)
//! - Each index = its own loop over its subscription; its serving gate = a separate task reading
//!   the quorum tip against the index's published applied height
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
use zaino_grpc::{GrpcLimits, GrpcServer, Tls, TlsFiles, TrustedProxies, ValidatorHandler};
use zaino_index_compact_block::{CompactBlockIndexWriter, CompactBlockService, CompactBlockStore};
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressService};
use zaino_index_tree_state::{
    PoolActivations, TreeStateIndexWriter, TreeStateService, TreeStateStore,
};
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashService, BlockHashStore};
use zaino_internal_value_balance::ValueBalanceIndexWriter;
use zaino_persistence::fs::{Fs, RealFs};
use zaino_primitives::types::{Block, ReorgDepth};
use zaino_source::{BlockFetchPool, FetchRoute, GetBlockchainInfo as _, ZebraRpcAdapter};
use zaino_sync::{BlockSink, FeeSink, Producer, Published, Subscription};
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
        source.into(),
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
    let started = std::time::Instant::now();
    // --- the chain view: quorum tip, mempool and broadcast fan-out, over every validator
    let chainview_span = crate::logging::component("ChainView");
    let chainview = crate::chainview::connect(Arc::clone(&validator), &config)
        .instrument(chainview_span.clone())
        .await?;
    let tips = chainview.handles.view.subscribe_tip();

    // --- the indexes: each its own files, its own finalised height, its own sink subscription
    //
    // A disabled index is never opened, subscribed or spawned, and its methods answer
    // UNIMPLEMENTED. compact-block and value-balance cannot be disabled (`DaemonConfig::validate`)
    let depth = ReorgDepth::new(config.fetch.finalised_depth);
    let mut block_sink = BlockSink::new("blocks");
    let fs = RealFs::shared();
    let (index, network) = (&config.index, config.network);

    // compact-block reads one fee step (value-balance's) per block step
    let mut fee_sink = FeeSink::new("fees");
    let (config_cb, config_vb) = (&index.compact_block, &index.value_balance);
    let (compact_block_span, store) = open_index(CompactBlockIndexWriter::NAME, config_cb, || {
        Ok(CompactBlockStore::open(Arc::clone(&fs), &config_cb.path, network)?)
    })?;
    let compact_block = CompactBlockIndexWriter::new(store, config_cb.batch_bytes());
    let compact_block_feeds = (
        block_sink.subscribe(CompactBlockIndexWriter::NAME, config_cb.queue_bytes()),
        fee_sink.subscribe(CompactBlockIndexWriter::NAME, config_vb.queue_bytes()),
    );
    let (value_balance_span, value_balance) =
        open_index(ValueBalanceIndexWriter::NAME, config_vb, || {
            let path = &config_vb.path;
            let batch = config_vb.batch_bytes();
            Ok(ValueBalanceIndexWriter::open(Arc::clone(&fs), path, network, batch)?)
        })?;
    let value_balance_blocks = subscribe(&mut block_sink, ValueBalanceIndexWriter::NAME, config_vb);
    let block_hash = open_block_hash(&fs, &index.block_hash, network)?;
    let tree_state = open_tree_state(&fs, &index.tree_state, network)?;
    let transparent = open_transparent_address(&fs, &index.transparent_address, network)?;
    let sink = &mut block_sink;
    let block_hash_blocks =
        block_hash.as_ref().map(|_| subscribe(sink, BlockHashIndexWriter::NAME, &index.block_hash));
    let tree_state_blocks =
        tree_state.as_ref().map(|_| subscribe(sink, TreeStateIndexWriter::NAME, &index.tree_state));
    let transparent_blocks = transparent
        .as_ref()
        .map(|_| subscribe(sink, TransparentAddressIndexWriter::NAME, &index.transparent_address));
    // every subscriber's durable tip (production starts after the rearmost)
    let durable = [
        Some(compact_block.durable_tip()),
        Some(value_balance.durable_tip()),
        block_hash.as_ref().map(|(_, index)| index.durable_tip()),
        tree_state.as_ref().map(|(_, index)| index.durable_tip()),
        transparent.as_ref().map(|(_, index)| index.durable_tip()),
    ];

    let compact_block_service = CompactBlockService::new(compact_block.published().served());
    let block_hash_service =
        block_hash.as_ref().map(|(_, index)| BlockHashService::new(index.published().served()));
    // pool activations = the validator's schedule, read once (never a compiled-in table)
    let tree_state_service = match &tree_state {
        Some((_, index)) => {
            let schedule = validator.get_blockchain_info().await?;
            let activations = PoolActivations::from_validator(&schedule);
            Some(TreeStateService::new(index.published().served(), network, activations))
        }
        None => None,
    };
    let transparent_service = transparent.as_ref().map(|(_, index)| {
        TransparentAddressService::new(index.published().served(), network)
            .with_max_rows(config.serve.max_address_rows)
    });

    // --- the producer: bulk over every validator (or the primary), then chainview's quorum tip
    let pool = BlockFetchPool::new(
        chainview.sources.clone(),
        config.primary_validator_index().map_or(FetchRoute::Spread, FetchRoute::Primary),
        config.fetch.concurrency,
    );
    let durable = durable.into_iter().flatten();
    let producer = Producer::new(block_sink, pool, tips.clone(), depth, durable)
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
    if let Some(tls) = &config.serve.tls {
        let files = TlsFiles { cert_path: tls.cert_path.clone(), key_path: tls.key_path.clone() };
        server = server.with_tls(Tls::load(files)?);
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

    // --- run: nothing fallible left, every stage one task
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let mut watchers =
        Watchers { tasks: &mut tasks, tips, depth, cancel: cancel.clone(), indexes: Vec::new() };
    let config_cb = &index.compact_block;
    let published = compact_block.published();
    watchers.watch(CompactBlockIndexWriter::NAME, &compact_block_span, published, config_cb, true);
    // no service reads value-balance (compact-block takes its fees through the sink)
    let (published, span) = (value_balance.published(), &value_balance_span);
    watchers.watch(ValueBalanceIndexWriter::NAME, span, published, &index.value_balance, false);
    if let Some((span, writer)) = &block_hash {
        let name = BlockHashIndexWriter::NAME;
        watchers.watch(name, span, writer.published(), &index.block_hash, true);
    }
    if let Some((span, writer)) = &tree_state {
        let name = TreeStateIndexWriter::NAME;
        watchers.watch(name, span, writer.published(), &index.tree_state, true);
    }
    if let Some((span, writer)) = &transparent {
        let name = TransparentAddressIndexWriter::NAME;
        watchers.watch(name, span, writer.published(), &index.transparent_address, true);
    }
    let disabled = [
        (BlockHashIndexWriter::NAME, block_hash.is_none()),
        (TreeStateIndexWriter::NAME, tree_state.is_none()),
        (TransparentAddressIndexWriter::NAME, transparent.is_none()),
    ];
    crate::status::publish(crate::status::Sources {
        network: crate::config::network_name(config.network),
        started,
        chainview: chainview.handles.view.clone(),
        fetched: producer.subscribe_fetched(),
        indexes: std::mem::take(&mut watchers.indexes),
        disabled: disabled.into_iter().filter_map(|(name, off)| off.then_some(name)).collect(),
    });

    // index loops: stopped by the producer's Shutdown (a failure panics)
    let (blocks, fees) = compact_block_feeds;
    let run = compact_block.run(blocks, fees);
    spawn_index(&mut tasks, "compact-block", compact_block_span, run);
    let run = value_balance.run(value_balance_blocks, fee_sink);
    spawn_index(&mut tasks, "value-balance", value_balance_span, run);
    if let (Some((span, index)), Some(blocks)) = (block_hash, block_hash_blocks) {
        spawn_index(&mut tasks, "block-hash", span, index.run(blocks));
    }
    if let (Some((span, index)), Some(blocks)) = (tree_state, tree_state_blocks) {
        spawn_index(&mut tasks, "tree-state", span, index.run(blocks));
    }
    if let (Some((span, index)), Some(blocks)) = (transparent, transparent_blocks) {
        spawn_index(&mut tasks, "transparent-address", span, index.run(blocks));
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

fn spawn_index(
    tasks: &mut JoinSet<TaskExit>,
    task: &'static str,
    component: Span,
    run: impl Future<Output = ()> + Send + 'static,
) {
    spawn(tasks, task, component, async move {
        run.await;
        Ok::<_, IndexerError>(())
    });
}

/// `open` under the index's component span, logged; the span then carries the index's task
fn open_index<W>(
    name: &str,
    config: &ZainoIndexConfig,
    open: impl FnOnce() -> Result<W, IndexerError>,
) -> Result<(Span, W), IndexerError> {
    let span = crate::logging::index_component(name);
    let opened = span.in_scope(|| {
        debug!("Opening from {}", crate::logging::shown_path(&config.path));
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
        Ok(BlockHashIndexWriter::new(store, config.batch_bytes()))
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
        Ok(TreeStateIndexWriter::new(store, config.batch_bytes())?)
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
        Ok(TransparentAddressIndexWriter::open(fs, &config.path, network, config.batch_bytes())?)
    };
    config
        .enabled
        .then(|| open_index(TransparentAddressIndexWriter::NAME, config, open))
        .transpose()
}

/// Index `name`'s own queue off `block_sink`
fn subscribe(
    block_sink: &mut BlockSink,
    name: &'static str,
    config: &ZainoIndexConfig,
) -> Subscription<Block> {
    block_sink.subscribe(name, config.queue_bytes())
}

/// What every index runs beside its loop: metrics, the status report, the serving gate
struct Watchers<'a> {
    tasks: &'a mut JoinSet<TaskExit>,
    tips: watch::Receiver<Option<QuorumTip>>,
    depth: ReorgDepth,
    cancel: CancellationToken,
    /// `/statusz` sources, published once every index is watched
    indexes: Vec<crate::status::IndexSource>,
}

impl Watchers<'_> {
    /// `served` = a service reads it (its request count reported)
    fn watch<V: Send + Sync + 'static>(
        &mut self,
        name: &'static str,
        span: &Span,
        published: &Published<V>,
        config: &ZainoIndexConfig,
        served: bool,
    ) {
        let watched = Watched {
            finalized: published.subscribe_finalized(),
            applied: published.subscribe_applied(),
            merged: published.subscribe_merged(),
            synced: published.subscribe_synced(),
            reads: served.then(|| published.reads()),
        };
        crate::metrics::track_index(name, &watched);
        let (measured, usage) = watch::channel(None);
        self.indexes.push(crate::status::IndexSource {
            name,
            finalized: watched.finalized.clone(),
            applied: watched.applied.clone(),
            merged: watched.merged.clone(),
            synced: watched.synced.clone(),
            reads: watched.reads.clone(),
            usage,
        });
        let report = crate::index_report::run(
            watched,
            config.path.clone(),
            measured,
            self.cancel.child_token(),
        );
        spawn(self.tasks, "index-report", span.clone(), report);
        let gate = published.gate(self.tips.clone(), self.depth, self.cancel.child_token());
        let gate = async move {
            gate.await;
            Ok::<(), IndexerError>(())
        };
        spawn(self.tasks, "serving-gate", span.clone(), gate);
    }
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
