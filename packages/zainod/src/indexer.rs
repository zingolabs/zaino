//! Boots the Zaino daemon and composes its pipeline.
//!
//! One fetch, N indexes, one server. The stack crates stay config-agnostic; this module is the
//! only place daemon config crosses into them, and the only place the pipeline's shape is
//! written down (`docs/design/sync.md`):
//!
//! ```text
//!   validators ──▶ HeaderSync ── VerifiedChain ─▶ Producer ──▶ BlockSink ─┬─▶ compact_block.run ◀──┐ fees
//!   validators ──▶ getblock <hash> (any, checked) ─┘                      ├─▶ value_balance.run    │
//!                                                                      │     └─▶ FeeSink ─────────┘
//!                                                                      ├─▶ block_hash.run
//!                                                                      ├─▶ tree_state.run
//!                                                                      ├─▶ transparent_address.run
//!                                                                      └─▶ (further indexes subscribe here)
//!
//!   non-finalized + files ──▶ CompactBlockService       ──┐
//!   non-finalized + files ──▶ BlockHashService          ──┤ (by-hash locator for the other two)
//!   non-finalized + files ──▶ TreeStateService          ──┤
//!   non-finalized + runs  ──▶ TransparentAddressService ──┼─▶ Routes ─▶ GrpcService
//!   ChainView (send, mempool, lightd info)              ──┤
//!   TrafficBalancer (GetTransaction bytes)              ──┘
//! ```
//!
//! - Stage → stage = a channel, wired here by hand (no scheduler, no dependency graph)
//! - Each index = its own loop over its subscription; its serving gate = a separate task reading
//!   the verified best against the index's published applied block (hash, not height)
//! - Every stage = one plain task in a `JoinSet`; fallible setup awaited before any spawn
//! - Scope: compact-block, block-hash, tree-state, transparent-address slices from their indexes,
//!   plus `SendTransaction`/`GetLightdInfo` off the validator

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _, Span};

use zaino_grpc::{GrpcLimits, GrpcService, Routes, Tls, TlsFiles, TrustedProxies};
use zaino_header_chain::VerifiedChain;
use zaino_index_compact_block::{CompactBlockIndexWriter, CompactBlockService};
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressService};
use zaino_index_tree_state::{PoolActivations, TreeStateIndexWriter, TreeStateService};
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashService};
use zaino_internal_value_balance::ValueBalanceIndexWriter;
use zaino_persistence::{fs::RealFs, DiskEngine, DiskStore, IndexKind, PersistenceEngine, Schema};
use zaino_primitives::network::network_name;
use zaino_primitives::types::{Block, BlockchainInfo, ReorgDepth};
use zaino_source::{ChainDataSource as _, Lane, TrafficBalancer, ZebraRpcAdapter};
use zaino_sync::{BlockSink, FeeSink, Producer, Published, Subscription};

use crate::config::{DaemonConfig, ShutdownConfig, ZainoIndexConfig};
use crate::error::IndexerError;
use crate::index_report::Watched;

/// Task name + outcome (name → log line for a task that ends early with `Ok`)
type TaskExit = (&'static str, Result<(), IndexerError>);

/// Start the Zaino daemon.
///
/// Returns a handle that resolves when the runtime exits: `Ok(())` on a shutdown signal, or the
/// first task's failure (the process then exits; nothing restarts in-process).
pub(crate) async fn start_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    warn!("In development, not for production mainnet use");
    spawn_indexer(config).await
}

/// Validate the config, then boot the runtime (no validator needs to answer first).
pub(crate) async fn spawn_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.validate()?;
    crate::fd_limit::raise_for(config.grpc.max_connections)?;
    boot(config).await
}

/// The upgrade schedule from whichever trusted validator answers first, asking until one does
///
/// - the one boot-time validator read (tree-state pool activations; never a compiled-in table)
async fn upgrade_schedule(validators: &[Arc<ZebraRpcAdapter>]) -> BlockchainInfo {
    let mut delay = std::time::Duration::from_secs(1);
    loop {
        for validator in validators {
            match validator.get_poll_reading(false, &[]).await {
                Ok(reading) => return reading.info,
                Err(error) => debug!(%error, "Validator not answering for the upgrade schedule"),
            }
        }
        warn!(retry = ?delay, "No trusted validator answering yet, waiting to read the upgrade schedule");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(std::time::Duration::from_secs(30));
    }
}

/// Compose the pipeline over the trusted validators, then spawn every stage.
///
/// - One connection pool per validator; chainview on `Lane::Control`, fetch on `Sync`, serving on
///   `Serve` (a lane never borrows another's connections)
async fn boot(config: DaemonConfig) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    let started = std::time::Instant::now();
    let fs = RealFs::shared();
    // --- the chain view: verified tip, mempool and submission, over every trusted validator
    let chainview_span = crate::logging::component("ChainView");
    let chainview =
        chainview_span.in_scope(|| crate::chainview::connect(&config, Arc::clone(&fs)))?;
    let view = chainview.view.subscriber();
    let verified = chainview.header_sync.subscribe();
    // serving's point lookups (`GetTransaction`), each to the least-loaded validator first
    let serve = chainview.sources.iter().map(|source| Arc::new(source.on(Lane::Serve))).collect();
    let validators = TrafficBalancer::new(serve);

    // --- the indexes: each its own files, its own finalised height, its own sink subscription
    //
    // A disabled index is never opened, subscribed or spawned, and its methods answer
    // UNIMPLEMENTED. compact-block and value-balance cannot be disabled (`DaemonConfig::validate`)
    let depth = ReorgDepth::new(config.fetch.finalised_depth);
    let mut block_sink = BlockSink::new("blocks");
    let (index, network) = (&config.index, config.network);

    // compact-block reads one fee step (value-balance's) per block step
    let mut fee_sink = FeeSink::new("fees");
    let (config_cb, config_vb) = (&index.compact_block, &index.value_balance);
    // the one engine every index stores through
    let engine = DiskEngine::new(Arc::clone(&fs));
    let schema = zaino_index_compact_block::schema(network);
    let (compact_block_span, compact_block) = open_index(&engine, config_cb, schema, |store| {
        Ok(CompactBlockIndexWriter::new(store, config_cb.batch_bytes()))
    })?;
    let compact_block_feeds = (
        block_sink.subscribe(IndexKind::CompactBlock.name(), config_cb.queue_bytes()),
        fee_sink.subscribe(IndexKind::CompactBlock.name(), config_vb.queue_bytes()),
    );
    let schema = zaino_internal_value_balance::schema(network);
    let (value_balance_span, value_balance) = open_index(&engine, config_vb, schema, |store| {
        Ok(ValueBalanceIndexWriter::new(store, config_vb.batch_bytes()))
    })?;
    let value_balance_blocks = subscribe(&mut block_sink, IndexKind::ValueBalance, config_vb);
    let schema = zaino_internal_block_hash_to_height::schema(network);
    let block_hash = open_optional(&engine, &index.block_hash, schema, |store, batch| {
        Ok(BlockHashIndexWriter::new(store, batch))
    })?;
    let schema = zaino_index_tree_state::schema(network);
    let tree_state = open_optional(&engine, &index.tree_state, schema, |store, batch| {
        Ok(TreeStateIndexWriter::new(store, batch))
    })?;
    let config_ta = &index.transparent_address;
    let schema = zaino_index_transparent_address::schema(network);
    let transparent = open_optional(&engine, config_ta, schema, |store, batch| {
        Ok(TransparentAddressIndexWriter::new(store, batch))
    })?;
    let sink = &mut block_sink;
    let block_hash_blocks =
        block_hash.as_ref().map(|_| subscribe(sink, IndexKind::BlockHash, &index.block_hash));
    let tree_state_blocks =
        tree_state.as_ref().map(|_| subscribe(sink, IndexKind::TreeState, &index.tree_state));
    let transparent_blocks =
        transparent.as_ref().map(|_| subscribe(sink, IndexKind::TransparentAddress, config_ta));
    // every subscriber's durable tip (production starts after the rearmost)
    let durable = [
        Some(compact_block.durable_tip()),
        Some(value_balance.durable_tip()),
        block_hash.as_ref().map(|(_, index)| index.durable_tip()),
        tree_state.as_ref().map(|(_, index)| index.durable_tip()),
        transparent.as_ref().map(|(_, index)| index.durable_tip()),
    ];

    let block_hash_service =
        block_hash.as_ref().map(|(_, index)| BlockHashService::new(index.published().served()));
    let tree_state_service = match &tree_state {
        Some((_, index)) => {
            let schedule =
                upgrade_schedule(&chainview.sources).instrument(chainview_span.clone()).await;
            let activations = PoolActivations::from_validator(&schedule);
            Some(TreeStateService::new(index.published().served(), network, activations))
        }
        None => None,
    };
    let transparent_service = transparent.as_ref().map(|(_, index)| {
        TransparentAddressService::new(index.published().served(), network)
            .with_max_rows(config.serve.max_address_rows)
    });

    // --- the producer: the verified chain, every block fetched from any validator and checked
    let sync = chainview.sources.iter().map(|source| Arc::new(source.on(Lane::Sync))).collect();
    let (concurrency, durable) = (config.fetch.concurrency, durable.into_iter().flatten());
    let producer = Producer::new(block_sink, sync, verified.clone(), concurrency, durable)
        .with_live_span(crate::logging::component("ZainoNFS"));

    // --- serving: the routes the config enabled; bound here (EADDRINUSE = boot failure)
    let routes = Routes {
        chain: Arc::clone(&chainview.view),
        validators,
        network,
        compact_block: CompactBlockService::new(compact_block.published().served()),
        block_hash: block_hash_service,
        tree_state: tree_state_service,
        transparent_address: transparent_service,
    };
    let limits = GrpcLimits::from(&config.grpc);
    let mut server = GrpcService::new(routes, config.serve.grpc_listen_address, limits)
        .with_trusted_proxies(TrustedProxies::new(config.grpc.trusted_proxies.clone()));
    if let Some(tls) = &config.serve.tls {
        let files = TlsFiles { cert_path: tls.cert_path.clone(), key_path: tls.key_path.clone() };
        server = server.with_tls(Tls::load(files)?);
    }
    let server = server.bind().await?;
    let grpc_span = crate::logging::component("Grpc");
    grpc_span.in_scope(|| {
        info!(
            endpoint = %config.serve.grpc_listen_address,
            network = network_name(config.network),
            "Listening"
        )
    });

    // --- run: nothing fallible left, every stage one task
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let mut watchers = Watchers {
        tasks: &mut tasks,
        verified,
        depth,
        cancel: cancel.clone(),
        indexes: Vec::new(),
    };
    let config_cb = &index.compact_block;
    let (published, name) = (compact_block.published(), IndexKind::CompactBlock.name());
    watchers.watch(name, &compact_block_span, published, config_cb, true);
    // no service reads value-balance (compact-block takes its fees through the sink)
    let (published, span) = (value_balance.published(), &value_balance_span);
    let name = IndexKind::ValueBalance.name();
    watchers.watch(name, span, published, &index.value_balance, false);
    if let Some((span, writer)) = &block_hash {
        let name = IndexKind::BlockHash.name();
        watchers.watch(name, span, writer.published(), &index.block_hash, true);
    }
    if let Some((span, writer)) = &tree_state {
        let name = IndexKind::TreeState.name();
        watchers.watch(name, span, writer.published(), &index.tree_state, true);
    }
    if let Some((span, writer)) = &transparent {
        let name = IndexKind::TransparentAddress.name();
        watchers.watch(name, span, writer.published(), &index.transparent_address, true);
    }
    let disabled = [
        (IndexKind::BlockHash.name(), block_hash.is_none()),
        (IndexKind::TreeState.name(), tree_state.is_none()),
        (IndexKind::TransparentAddress.name(), transparent.is_none()),
    ];
    crate::status::publish(crate::status::Sources {
        network: network_name(config.network),
        started,
        chainview: view,
        fetched: producer.subscribe_fetched(),
        indexes: std::mem::take(&mut watchers.indexes),
        disabled: disabled.into_iter().filter_map(|(name, off)| off.then_some(name)).collect(),
    });

    // index loops: stopped by the producer's Shutdown (a failure panics)
    let (blocks, fees) = compact_block_feeds;
    let run = compact_block.run(blocks, fees);
    spawn_index(&mut tasks, IndexKind::CompactBlock, compact_block_span, run);
    let run = value_balance.run(value_balance_blocks, fee_sink);
    spawn_index(&mut tasks, IndexKind::ValueBalance, value_balance_span, run);
    if let (Some((span, index)), Some(blocks)) = (block_hash, block_hash_blocks) {
        spawn_index(&mut tasks, IndexKind::BlockHash, span, index.run(blocks));
    }
    if let (Some((span, index)), Some(blocks)) = (tree_state, tree_state_blocks) {
        spawn_index(&mut tasks, IndexKind::TreeState, span, index.run(blocks));
    }
    if let (Some((span, index)), Some(blocks)) = (transparent, transparent_blocks) {
        spawn_index(&mut tasks, IndexKind::TransparentAddress, span, index.run(blocks));
    }
    let token = cancel.child_token();
    let run = chainview.header_sync.run(token);
    spawn(&mut tasks, "header-sync", chainview_span.clone(), run);
    if let Some((starting, watch)) = chainview.peers {
        // one task, ended only by cancel (a task ending = a fault): the start finishes early
        let token = cancel.child_token();
        let run = async move {
            tokio::join!(token.run_until_cancelled(starting), watch.run(token.clone()));
            Ok::<_, IndexerError>(())
        };
        spawn(&mut tasks, "peers", chainview_span.clone(), run);
    }
    for (poller, watch) in chainview.pollers {
        if let Some(watch) = watch {
            let (woken, linked) = (poller.waker(), poller.waker());
            let token = cancel.child_token();
            let run = async move {
                watch.run(token, move |_| woken.wake(), move |up| linked.streaming(up)).await;
                Ok::<_, IndexerError>(())
            };
            spawn(&mut tasks, "push-stream", chainview_span.clone(), run);
        }
        let token = cancel.child_token();
        let run = async move {
            poller.run(token).await;
            Ok::<_, IndexerError>(())
        };
        spawn(&mut tasks, "chainview", chainview_span.clone(), run);
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

    let shutdown = config.grpc.shutdown.clone();
    Ok(tokio::spawn(supervise(tasks, cancel, shutdown_signals(), shutdown)))
}

/// Signal → drain → `Ok(())`; else the first failure (ending cleanly before shutdown is one)
///
/// - Either way: cancel the rest, then wait for them (followers flush what is final)
async fn supervise(
    mut tasks: JoinSet<TaskExit>,
    cancel: CancellationToken,
    mut signals: mpsc::Receiver<&'static str>,
    shutdown: ShutdownConfig,
) -> Result<(), IndexerError> {
    let mut failure = tokio::select! {
        Some(signal) = signals.recv() => {
            info!(signal, "Shutdown signal received");
            drain(&mut tasks, &mut signals, &shutdown).await
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

/// `/readyz` fails with `draining` while everything keeps serving for `shutdown.delay()` (a
/// load balancer polling readiness stops routing here before the listener closes)
///
/// - cut short by a second signal; a task ending meanwhile = the failure it always was
async fn drain(
    tasks: &mut JoinSet<TaskExit>,
    signals: &mut mpsc::Receiver<&'static str>,
    shutdown: &ShutdownConfig,
) -> Option<IndexerError> {
    let delay = shutdown.delay();
    crate::status::drain();
    crate::notify::stopping(delay + shutdown.timeout());
    if delay.is_zero() {
        return None;
    }
    info!(?delay, "Draining, still serving");
    tokio::select! {
        () = tokio::time::sleep(delay) => None,
        Some(signal) = signals.recv() => {
            info!(signal, "Second signal, drain cut short");
            None
        }
        Some(exit) = tasks.join_next() => Some(first_failure(exit)),
    }
}

/// A task ended before shutdown (or during its drain): always a fault
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
    index: IndexKind,
    component: Span,
    run: impl Future<Output = ()> + Send + 'static,
) {
    spawn(tasks, index.name(), component, async move {
        run.await;
        Ok::<_, IndexerError>(())
    });
}

/// `config.path` opened as `schema`'s store and handed to `writer`, under the index's component
/// span, logged; the span then carries the index's task
fn open_index<W>(
    engine: &DiskEngine,
    config: &ZainoIndexConfig,
    schema: Schema,
    writer: impl FnOnce(DiskStore) -> Result<W, IndexerError>,
) -> Result<(Span, W), IndexerError> {
    let span = crate::logging::index_component(schema.kind.name());
    let opened = span.in_scope(|| {
        debug!("Opening from {}", crate::logging::shown_path(&config.path));
        writer(engine.open(&config.path, &schema)?)
    })?;
    Ok((span, opened))
}

/// Enabled → [`open_index`] (`writer` given the batch size); disabled → `None`, `config.path`
/// never created
fn open_optional<W>(
    engine: &DiskEngine,
    config: &ZainoIndexConfig,
    schema: Schema,
    writer: impl FnOnce(DiskStore, NonZeroUsize) -> Result<W, IndexerError>,
) -> Result<Option<(Span, W)>, IndexerError> {
    let open = |store| writer(store, config.batch_bytes());
    config.enabled.then(|| open_index(engine, config, schema, open)).transpose()
}

/// `index`'s own queue off `block_sink`
fn subscribe(
    block_sink: &mut BlockSink,
    index: IndexKind,
    config: &ZainoIndexConfig,
) -> Subscription<Block> {
    block_sink.subscribe(index.name(), config.queue_bytes())
}

/// What every index runs beside its loop: metrics, the status report, the serving gate
struct Watchers<'a> {
    tasks: &'a mut JoinSet<TaskExit>,
    verified: watch::Receiver<Option<Arc<VerifiedChain>>>,
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
        let gate = published.gate(self.verified.clone(), self.depth, self.cancel.child_token());
        let gate = async move {
            gate.await;
            Ok::<(), IndexerError>(())
        };
        spawn(self.tasks, "serving-gate", span.clone(), gate);
    }
}

/// Every shutdown signal, named (the first starts the drain, a second cuts it short)
///
/// - Handlers registered here, before boot returns (from then on SIGTERM never kills outright)
fn shutdown_signals() -> mpsc::Receiver<&'static str> {
    let (sender, receiver) = mpsc::channel(1);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Registering a signal handler only fails on a broken runtime/OS, which
        // is an unrecoverable process-level invariant, not a runtime condition.
        let mut terminate = signal(SignalKind::terminate()).expect("register SIGTERM handler");
        let mut interrupt = signal(SignalKind::interrupt()).expect("register SIGINT handler");
        tokio::spawn(async move {
            loop {
                let signal = tokio::select! {
                    _ = interrupt.recv() => "SIGINT",
                    _ = terminate.recv() => "SIGTERM",
                };
                if sender.send(signal).await.is_err() {
                    return;
                }
            }
        });
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if sender.send("ctrl-c").await.is_err() {
                return;
            }
        }
    });
    receiver
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

            let (_no_signal, signals) = mpsc::channel(1);
            let outcome =
                supervise(tasks, cancel.clone(), signals, ShutdownConfig::default()).await;

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

    /// A signal with `[grpc.shutdown]` on fails readiness at once while every task keeps
    /// serving, until the delay runs out, a second signal cuts it short, or a task ending
    /// meanwhile fails the daemon; only then is the rest cancelled
    #[tokio::test(start_paused = true)]
    async fn a_signal_drains_while_serving_until_the_delay_a_second_signal_or_a_failure() {
        let secs = std::time::Duration::from_secs;
        let shutdown = ShutdownConfig { enabled: true, delay_secs: 60, timeout_secs: 0 };
        // case, second signal at, a task ending at, supervise returns at, Ok
        let cases = [
            ("delay runs out", None, None, secs(60), true),
            ("second signal", Some(secs(15)), None, secs(15), true),
            ("task ends", None, Some(secs(20)), secs(20), false),
        ];

        for (case, second, ends, returns, ok) in cases {
            let cancel = CancellationToken::new();
            let mut tasks = JoinSet::new();
            let token = cancel.child_token();
            spawn(&mut tasks, "serving", Span::none(), async move {
                token.cancelled().await;
                Ok::<_, IndexerError>(())
            });
            if let Some(at) = ends {
                spawn(&mut tasks, "early", Span::none(), async move {
                    tokio::time::sleep(at).await;
                    Ok::<_, IndexerError>(())
                });
            }
            let (signal, signals) = mpsc::channel(1);
            let started = tokio::time::Instant::now();
            let supervising =
                tokio::spawn(supervise(tasks, cancel.clone(), signals, shutdown.clone()));

            signal.send("SIGTERM").await.expect("supervise receives");
            tokio::time::sleep(secs(5)).await;
            assert!(crate::status::draining(), "{case}: readiness still passing");
            assert!(!cancel.is_cancelled(), "{case}: stopped serving inside the drain");
            if let Some(at) = second {
                tokio::time::sleep(at - secs(5)).await;
                signal.send("SIGINT").await.expect("supervise receives");
            }

            let outcome = supervising.await.expect("supervise ran");
            assert_eq!(started.elapsed(), returns, "{case}");
            assert_eq!(outcome.is_ok(), ok, "{case}: {outcome:?}");
            assert!(cancel.is_cancelled(), "{case}: rest not cancelled");
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

        let engine = DiskEngine::new(RealFs::shared());
        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let optional = [
            ("block-hash", zaino_internal_block_hash_to_height::schema(net)),
            ("tree-state", zaino_index_tree_state::schema(net)),
            ("transparent", zaino_index_transparent_address::schema(net)),
        ];
        for (name, schema) in optional {
            for enabled in [false, true] {
                let config = index(&format!("{name}-{enabled}"), enabled);
                let opened = open_optional(&engine, &config, schema.clone(), |store, _| Ok(store));
                let opened = opened.expect("opens when enabled").is_some();
                let created = config.path.exists();
                assert_eq!((opened, created), (enabled, enabled), "{name}, enabled = {enabled}");
            }
        }
    }
}
