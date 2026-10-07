//! Daemon boot + pipeline composition (`docs/design/nfs.md` §7)
//!
//! - Only place daemon config crosses into the (config-agnostic) stack crates
//! - Only place the pipeline's shape is written down:
//!
//! ```text
//!   validators ──▶ HeaderSync ── VerifiedChain ─▶ Nfs ──▶ final stream ─┬─▶ value_balance ─fees─┐
//!   validators ──▶ getblock <hash> (any, checked) ┘  │                  ├─▶ compact_block ◀──────┘
//!                                                    │                  ├─▶ block_hash
//!                                                    │                  ├─▶ tree_state
//!                                                    │                  └─▶ transparent_address
//!                                                    │   ◀── each writer's committed view ──┘
//!                                                    ▼
//!                                        NfsHandle ─▶ snapshot ─▶ Routes ─▶ GrpcService
//!   ChainView (send, mempool, lightd info) + TrafficBalancer (GetTransaction) ─┘
//! ```
//!
//! - One NFS: fetch, fold at the tip, the final stream, one snapshot per request
//! - Each writer = its own task over its subscription; a disabled index is never opened
//! - Every stage = one plain task in a `JoinSet`; the first to end ends the daemon

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _, Span};

use zaino_chainview::ChainView;
use zaino_grpc::{GrpcLimits, GrpcService, Routes, Tls, TlsFiles, TrustedProxies};
use zaino_header_chain::VerifiedChain;
use zaino_index_compact_block::{self as compact_block, CompactBlockIndexWriter};
use zaino_index_transparent_address::{self as transparent_address, TransparentAddressIndexWriter};
use zaino_index_tree_state::{self as tree_state, PoolActivations, TreeStateIndexWriter};
use zaino_internal_block_hash_to_height::{self as block_hash, BlockHashIndexWriter};
use zaino_internal_value_balance::{self as value_balance, ValueBalanceIndexWriter};
use zaino_nfs::{ChainParams, Nfs};
use zaino_persistence::fs::{Fs, RealFs};
use zaino_persistence::{DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, Schema};
use zaino_primitives::network::network_name;
use zaino_primitives::types::{BlockchainInfo, ReorgDepth};
use zaino_source::ChainDataSource;
use zaino_sync::{FeeSink, Final, Subscription};
use zaino_traffic::{Push, TrafficBalancer, ValidatorId};

use crate::config::{DaemonConfig, IndexConfig, ShutdownConfig};
use crate::error::IndexerError;
use crate::logging::component;

/// Task name + outcome (name → log line for a task that ends early with `Ok`)
type TaskExit = (&'static str, Result<(), IndexerError>);

/// Handle → `Ok(())` on a shutdown signal, else the first task's failure (process exits; nothing
/// restarts in-process)
pub(crate) async fn start_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    warn!("In development, not for production mainnet use");
    spawn_indexer(config).await
}

/// Config validated, then boot (no validator needs to answer first)
pub(crate) async fn spawn_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.validate()?;
    crate::fd_limit::raise_for(config.grpc.max_connections)?;
    boot(config).await
}

/// Upgrade schedule from the first poll any trusted validator answers (catching up counts: its
/// schedule is the network's)
///
/// - the one boot-time validator read (tree-state pool activations; never a compiled-in table)
async fn upgrade_schedule<S: ChainDataSource>(
    balancer: &TrafficBalancer<S>,
    validators: usize,
) -> BlockchainInfo {
    let members = (0..validators).filter_map(ValidatorId::new);
    let mut observed: Vec<_> = members.map(|member| balancer.observe(member)).collect();
    let mut warned = false;
    loop {
        for observation in &mut observed {
            let observation = observation.borrow_and_update().clone();
            match observation.as_ref().map(|observation| &observation.polled) {
                Some(Ok(reading)) => return reading.info.clone(),
                Some(Err(cause)) if !warned => {
                    warn!(%cause, "No trusted validator answering yet, waiting to read the upgrade schedule");
                    warned = true;
                }
                Some(Err(_)) | None => {}
            }
        }
        let changes = observed.iter_mut().map(|observation| Box::pin(observation.changed()));
        let (changed, ..) = futures::future::select_all(changes).await;
        changed.expect("the balancer outlives boot");
    }
}

/// Chain view over the trusted validators → pipeline over it → every task spawned
///
/// - One balancer over every validator; its driver first (the upgrade schedule is a poll's)
async fn boot(config: DaemonConfig) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    let started = std::time::Instant::now();
    let fs = RealFs::shared();
    let chainview_span = component("ChainView");
    let chainview =
        chainview_span.in_scope(|| crate::chainview::connect(&config, Arc::clone(&fs)))?;
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let balancing = chainview.balancing.run(cancel.child_token());
    spawn_infallible(&mut tasks, "traffic", component("Traffic"), balancing);
    let validators = config.trusted_validators.len();
    let schedule = upgrade_schedule(&chainview.balancer, validators);
    let schedule = schedule.instrument(chainview_span.clone()).await;
    let inputs = Inputs {
        chain: chainview.header_sync.subscribe(),
        view: Arc::clone(&chainview.view),
        balancer: chainview.balancer.clone(),
        activations: PoolActivations::from_validator(&schedule),
    };
    let mut tasks = pipeline(&config, fs, inputs, &cancel, started, tasks).await?;

    let run = chainview.header_sync.run(cancel.child_token());
    spawn(&mut tasks, "header-sync", chainview_span.clone(), run);
    if let Some((starting, watch)) = chainview.peers {
        // one task, ended only by cancel (a task ending = a fault): the start finishes early
        let token = cancel.child_token();
        let run = async move {
            tokio::join!(token.run_until_cancelled(starting), watch.run(token.clone()));
        };
        spawn_infallible(&mut tasks, "peers", chainview_span.clone(), run);
    }
    let fold = chainview.fold.run(cancel.child_token());
    spawn_infallible(&mut tasks, "chainview", chainview_span.clone(), fold);
    for (member, watch) in chainview.watches {
        let (changed, linked) = (chainview.balancer.clone(), chainview.balancer.clone());
        let on_change = move |_| changed.pushed(member, Push::Changed);
        let on_link = move |up| linked.pushed(member, Push::Link(up));
        let run = watch.run(cancel.child_token(), on_change, on_link);
        spawn_infallible(&mut tasks, "push-stream", chainview_span.clone(), run);
    }
    let heartbeat = crate::admin::beat(cancel.child_token());
    spawn(&mut tasks, "heartbeat", component("Metrics"), heartbeat);

    let shutdown = config.grpc.shutdown.clone();
    Ok(tokio::spawn(supervise(tasks, cancel, shutdown_signals(), shutdown)))
}

/// Pipeline inputs (production: header sync + trusted validators; tests: mocks)
struct Inputs<S: ChainDataSource> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    view: Arc<ChainView<S>>,
    balancer: TrafficBalancer<S>,
    activations: PoolActivations,
}

/// The NFS, every enabled index's writer, the gRPC server: opened, subscribed, bound, spawned
/// into `tasks`
///
/// - an early `Err` leaves nothing running (`tasks` dropped before the NFS: no writer sees its
///   stream end)
async fn pipeline<S: ChainDataSource>(
    config: &DaemonConfig,
    fs: Arc<dyn Fs>,
    inputs: Inputs<S>,
    cancel: &CancellationToken,
    started: std::time::Instant,
    tasks: JoinSet<TaskExit>,
) -> Result<JoinSet<TaskExit>, IndexerError> {
    let network = config.network;
    let depth = ReorgDepth::new(config.sync.finalised_depth);
    let params = ChainParams { network, activations: inputs.activations };
    let verified = inputs.chain.clone();
    let balancer = inputs.balancer.clone();
    let nfs = Nfs::new(inputs.chain, balancer, params, config.sync.concurrency, depth);
    let mut indexes = Subscribed { nfs, opened: Vec::new() };
    // declared after the NFS: dropped first
    let mut tasks = tasks;
    let engine = DiskEngine::new(fs);

    if let Some((cb, vb)) = config.compact_block()? {
        // compact-block folds after value-balance (its fees)
        let mut fee_sink = FeeSink::new("fees");
        let fees = fee_sink.subscribe(IndexKind::CompactBlock.name(), cb.queue_bytes);
        let schema = value_balance::schema(network);
        let (span, writer) = open(&engine, &vb, schema, ValueBalanceIndexWriter::new)?;
        let blocks = indexes.subscribe(IndexKind::ValueBalance, writer.committed(), &vb, &span);
        let run = writer.run(blocks, fee_sink);
        spawn_infallible(&mut tasks, IndexKind::ValueBalance.name(), span, run);
        let schema = compact_block::schema(network);
        let (span, writer) = open(&engine, &cb, schema, CompactBlockIndexWriter::new)?;
        let blocks = indexes.subscribe(IndexKind::CompactBlock, writer.committed(), &cb, &span);
        let run = writer.run(blocks, fees);
        spawn_infallible(&mut tasks, IndexKind::CompactBlock.name(), span, run);
    }
    if let Some(bh) = config.enabled(IndexKind::BlockHash) {
        let schema = block_hash::schema(network);
        let (span, writer) = open(&engine, &bh, schema, BlockHashIndexWriter::new)?;
        let blocks = indexes.subscribe(IndexKind::BlockHash, writer.committed(), &bh, &span);
        spawn_infallible(&mut tasks, IndexKind::BlockHash.name(), span, writer.run(blocks));
    }
    if let Some(ts) = config.enabled(IndexKind::TreeState) {
        let schema = tree_state::schema(network);
        let (span, writer) = open(&engine, &ts, schema, TreeStateIndexWriter::new)?;
        let blocks = indexes.subscribe(IndexKind::TreeState, writer.committed(), &ts, &span);
        spawn_infallible(&mut tasks, IndexKind::TreeState.name(), span, writer.run(blocks));
    }
    if let Some(ta) = config.enabled(IndexKind::TransparentAddress) {
        let kind = IndexKind::TransparentAddress;
        let schema = transparent_address::schema(network);
        let (span, writer) = open(&engine, &ta, schema, TransparentAddressIndexWriter::new)?;
        let blocks = indexes.subscribe(kind, writer.committed(), &ta, &span);
        spawn_infallible(&mut tasks, kind.name(), span, writer.run(blocks));
    }
    let snapshots = indexes.nfs.handle();
    let members = inputs.balancer.members();

    // --- serving: bound here (EADDRINUSE = boot failure), every answer off one snapshot
    let routes = Routes {
        chain: Arc::clone(&inputs.view),
        validators: inputs.balancer,
        network,
        nfs: snapshots.clone(),
        max_address_rows: config.serve.max_address_rows,
    };
    let limits = GrpcLimits::from(&config.grpc);
    let mut server = GrpcService::new(routes, config.serve.grpc_listen_address, limits)
        .with_trusted_proxies(TrustedProxies::new(config.grpc.trusted_proxies.clone()));
    if let Some(tls) = &config.serve.tls {
        let files = TlsFiles { cert_path: tls.cert_path.clone(), key_path: tls.key_path.clone() };
        server = server.with_tls(Tls::load(files)?);
    }
    let server = server.bind().await?;
    let grpc_span = component("Grpc");
    let endpoint = config.serve.grpc_listen_address;
    grpc_span.in_scope(|| info!(%endpoint, network = network_name(network), "Listening"));

    // --- run: nothing fallible left
    let Subscribed { nfs, opened } = indexes;
    let handed = nfs.subscribe_handed();
    spawn(&mut tasks, "nfs", component("ZainoNFS"), nfs.run(cancel.child_token()));
    spawn(&mut tasks, "grpc", grpc_span, server.run(cancel.child_token()));
    let synced = watch::Sender::new(false);
    let judged = crate::serving::run(
        snapshots.clone(),
        verified,
        depth,
        synced.clone(),
        cancel.child_token(),
    );
    spawn(&mut tasks, "serving", component("ZainoNFS"), judged);

    let mut sources = Vec::new();
    for Opened { kind, span, path, committed } in opened {
        let watched = crate::index_report::Watched {
            committed: committed.clone(),
            synced: synced.subscribe(),
        };
        crate::metrics::track_index(kind.name(), &watched);
        let (measured, usage) = watch::channel(None);
        let report = crate::index_report::run(watched, path, measured, cancel.child_token());
        spawn(&mut tasks, "index-report", span, report);
        sources.push(crate::status::IndexSource { name: kind.name(), committed, usage });
    }
    let off = [IndexKind::CompactBlock, IndexKind::BlockHash, IndexKind::TreeState];
    let off = off.into_iter().chain([IndexKind::TransparentAddress]);
    let disabled = off.filter(|&kind| config.enabled(kind).is_none()).map(IndexKind::name);
    crate::status::publish(crate::status::Sources {
        network: network_name(network),
        started,
        chainview: inputs.view.subscriber(),
        members,
        handed,
        served: snapshots,
        synced: synced.subscribe(),
        indexes: sources,
        disabled: disabled.collect(),
    });
    Ok(tasks)
}

/// The NFS + every index subscribed to it so far (each watched for status once spawned)
struct Subscribed<S> {
    nfs: Nfs<S, DiskView>,
    opened: Vec<Opened>,
}

struct Opened {
    kind: IndexKind,
    span: Span,
    path: std::path::PathBuf,
    committed: watch::Receiver<DiskView>,
}

impl<S: ChainDataSource> Subscribed<S> {
    /// `kind` enabled: its committed view in, its final stream out
    fn subscribe(
        &mut self,
        kind: IndexKind,
        committed: watch::Receiver<DiskView>,
        config: &IndexConfig,
        span: &Span,
    ) -> Subscription<Final> {
        let blocks = self.nfs.subscribe(kind, committed.clone(), config.queue_bytes);
        let (span, path) = (span.clone(), config.path.clone());
        self.opened.push(Opened { kind, span, path, committed });
        blocks
    }
}

/// `config.path` opened as `schema`'s store and handed to `writer` with the batch size, under
/// the index's component span (logged); the span then carries the index's task
fn open<W>(
    engine: &DiskEngine,
    config: &IndexConfig,
    schema: Schema,
    writer: impl FnOnce(DiskStore, NonZeroUsize) -> W,
) -> Result<(Span, W), IndexerError> {
    let span = crate::logging::index_component(schema.kind.name());
    let opened = span.in_scope(|| {
        debug!("Opening from {}", crate::logging::shown_path(&config.path));
        engine.open(&config.path, &schema).map(|store| writer(store, config.batch_bytes))
    })?;
    Ok((span, opened))
}

/// Signal → drain → `Ok(())`; else the first failure (ending cleanly before shutdown is one)
///
/// - Either way: cancel the rest, then wait for them (writers commit what they hold)
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

/// `/readyz` = `draining`, everything still serving, for `shutdown.delay()` (a polling load
/// balancer stops routing here before the listener closes)
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

/// [`spawn`] for a task with no failure of its own
fn spawn_infallible(
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

/// Every shutdown signal, named (the first starts the drain, a second cuts it short)
///
/// - Handlers registered here, before boot returns (from then on SIGTERM never kills outright)
fn shutdown_signals() -> mpsc::Receiver<&'static str> {
    let (sender, receiver) = mpsc::channel(1);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Fails only on a broken runtime/OS (unrecoverable process-level invariant)
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
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// - First task to end = the daemon's failure, named (clean end, own error, panic)
    /// - Returned only after every other task saw the cancel and finished
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

    /// - Signal + `[grpc.shutdown]` on → readiness fails at once, every task still serving
    /// - Until: delay out, a second signal, or a task ending (= failure); only then the cancel
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

    /// Whole pipeline over a mock validator (indexes on `SimFs`, gRPC on localhost, depth 3):
    /// - A 0..=8 at once (0..=5 final: bulk, committed on idle; 6..=8 folded at the tip)
    /// - then B7 (heavier, off A6) + B8 → reorged heights serve B's
    /// - before + after: `GetLatestBlock`, `GetBlockRange`'s last, `GetTreeState` = one block
    /// - cancel stops every task cleanly
    #[tokio::test]
    async fn the_pipeline_follows_a_reorg_and_every_rpc_agrees_on_the_served_tip() {
        use std::num::NonZeroU32;
        use std::time::Duration;

        use zaino_header_chain::HeaderChain;
        use zaino_primitives::testing::Chain;
        use zaino_primitives::types::{Block, BlockHash, Height};
        use zaino_proto::proto::service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
        };
        use zaino_source::mock::MockChain;

        let mut blocks = Chain::new();
        let genesis = blocks.genesis().hash;
        let a8 = blocks.extend(genesis, 8);
        let a: Vec<Block> = blocks.path(a8.hash);
        let hash = |block: &Block| block.header().hash;
        let b7 = blocks.mine_heavier(hash(&a[6]), &[hash(&a[7]), hash(&a[8])]).expect("in range");
        let b8 = blocks.mine(b7.hash);
        let b: Vec<Block> = blocks.path(b8.hash)[7..].to_vec();

        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut headers = HeaderChain::regtest_in_memory(genesis, depth);
        let (verified, chain) = watch::channel(None);
        let mock = Arc::new(MockChain::serving(a.clone()));
        let genesis_height = Height::GENESIS;
        let activations = PoolActivations {
            sapling: genesis_height,
            orchard: Some(genesis_height),
            ironwood: Some(genesis_height),
        };
        let limits = zaino_traffic::Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = zaino_traffic::Trusted { source: Arc::clone(&mock), priority: 0, limits };
        let (balancer, balancing) = TrafficBalancer::new(vec![trusted], None);
        let view = ChainView::new(vec!["mock".to_owned()], balancer.clone(), depth);
        let view = Arc::new(view.expect("one endpoint"));
        let inputs = Inputs { chain, view, balancer, activations };
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let address = probe.local_addr().expect("local addr");
        drop(probe);
        let mut config = DaemonConfig {
            network: zcash_protocol::consensus::NetworkType::Regtest,
            ..DaemonConfig::default()
        };
        config.sync.finalised_depth = NonZeroU32::new(3).expect("non-zero");
        config.serve.grpc_listen_address = address;
        let fs = zaino_persistence::fs::SimFs::new();
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let mut tasks = JoinSet::new();
        spawn_infallible(&mut tasks, "traffic", Span::none(), balancing.run(cancel.child_token()));
        let tasks = pipeline(&config, fs, inputs, &cancel, started, tasks);
        let tasks = tasks.await.expect("pipeline up");
        let mut wallet = CompactTxStreamerClient::connect(format!("http://{address}"))
            .await
            .expect("the gRPC listener is bound");

        let mut publish = |added: &[Block]| {
            headers.insert_blocks(added).expect("valid headers");
            if let Some(boundary) = headers.finalizable() {
                headers.finalize(boundary).expect("in-memory store");
            }
            verified.send_replace(headers.verified().map(Arc::new));
        };
        // (latest, range's last, tree state at it): each the block's (height, hash)
        let mut agree_at = async |tip: &Block| {
            let expected = (u64::from(tip.header().height), <[u8; 32]>::from(tip.header().hash));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                let latest = match wallet.get_latest_block(ChainSpec {}).await {
                    Ok(latest) => Some(latest.into_inner()),
                    Err(syncing) if syncing.code() == tonic::Code::Unavailable => None,
                    Err(status) => panic!("GetLatestBlock: {status}"),
                };
                let served = latest.as_ref().map(|id| (id.height, id.hash.as_slice()));
                if served == Some((expected.0, &expected.1[..])) {
                    break;
                }
                assert!(tokio::time::Instant::now() < deadline, "served {latest:?}, want {tip:?}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let at = |height| Some(BlockId { height, hash: Vec::new() });
            let range = BlockRange { start: at(0), end: at(99), pool_types: Vec::new() };
            let mut stream = wallet.get_block_range(range).await.expect("range").into_inner();
            let mut last = None;
            while let Some(block) = stream.message().await.expect("a block") {
                last = Some((block.height, block.hash));
            }
            assert_eq!(last, Some((expected.0, expected.1.to_vec())), "range ends at the tip");
            let state = wallet.get_tree_state(BlockId { height: expected.0, hash: Vec::new() });
            let state = state.await.expect("tree state").into_inner();
            assert_eq!(state.hash, BlockHash::from(expected.1).to_string(), "tree state's block");
        };

        publish(&a);
        agree_at(&a[8]).await;
        mock.extend_best(b.clone());
        publish(&b);
        agree_at(&b[1]).await;
        let reorged = wallet.get_block(BlockId { height: 7, hash: Vec::new() }).await;
        let reorged = reorged.expect("block 7").into_inner().hash;
        assert_eq!(reorged, <[u8; 32]>::from(hash(&b[0])).to_vec(), "7 = B7 now");

        cancel.cancel();
        let (_no_signal, signals) = mpsc::channel(1);
        supervise(tasks, cancel, signals, ShutdownConfig::default()).await.expect("clean stop");
    }
}
