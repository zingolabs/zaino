//! Daemon boot + pipeline composition (`docs/architecture.md` §2)
//!
//! - Only place daemon config crosses into the (config-agnostic) stack crates
//! - Only place the pipeline's shape is written down:
//!
//! ```text
//!   validators ──▶ HeaderSync ── VerifiedChain ─┬─▶ FinalFollower ─▶ final stream ─┬─▶ value_balance ─fees─┐
//!   validators ──▶ getblock <hash> (checked) ───┤                                  ├─▶ compact_block ◀──────┘
//!                                               │                                  ├─▶ block_hash
//!                                               │                                  ├─▶ tree_state
//!                                               │                                  └─▶ transparent_address
//!                                               └─▶ Nfs (tip overlay) ◀── each writer's IndexHandle ──┘
//!                                                    ▼
//!                                   Indexed ─┐
//!   ChainView (holders, mempool, facts) ─────┴─▶ Publisher ─▶ Snapshots ─▶ Routes ─▶ GrpcService
//!   ChainView (send) + TrafficBalancer (GetTransaction) ─────────────────────┘    └▶ admin, logs
//! ```
//!
//! - Final path: follower → writers (disk); tip path: NFS (RAM); one global snapshot per request
//! - Each writer = its own task over its subscription; a disabled index is never opened
//! - Every stage = one plain task in a `JoinSet`; the first to end ends the daemon

use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _, Span};

use zaino_chainview::ChainView;
use zaino_grpc::network_name;
use zaino_grpc::{GrpcLimits, GrpcService, Load, Routes, Routing, Tls, TlsFiles, TrustedProxies};
use zaino_header_chain::{Params, VerifiedChain};
use zaino_index_compact_block::CompactBlockIndexWriter;
use zaino_index_transparent_address::TransparentAddressIndexWriter;
use zaino_index_tree_state::TreeStateIndexWriter;
use zaino_internal_block_hash_to_height::BlockHashIndexWriter;
use zaino_internal_value_balance::ValueBalanceIndexWriter;
use zaino_nfs::{ChainParams, Nfs};
use zaino_persistence::fs::{Fs, RealFs};
use zaino_persistence::{DiskEngine, DiskStore, DiskView, IndexKind, LsmConfig, PersistenceEngine};
use zaino_primitives::protocol::compiled_schedule;
use zaino_primitives::types::{Block, ReorgDepth};
use zaino_snapshot::Publisher;
use zaino_source::{BlockchainInfo, ChainDataSource};
use zaino_sync::{FeeSink, FinalFollower, IndexHandle, Subscription};
use zaino_traffic::{Push, TrafficBalancer, ValidatorId};
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::local_consensus::LocalNetwork;

use crate::config::{DaemonConfig, IndexConfig, TrustedValidatorConfig, ValidatorClient};
use crate::error::IndexerError;
use crate::logging::component;
use crate::stores;

/// Task name + outcome (name → log line for a task that ends early with `Ok`)
type TaskExit = (&'static str, Result<(), IndexerError>);

/// Handle → `Ok(())` on a shutdown signal, else the first task's failure (process exits; nothing
/// restarts in-process)
///
/// - gRPC listener first: health answers through the snapshot bootstrap, a taken port fails
///   before any download
/// - then the bootstrap, then boot (no validator needs to answer first)
pub(crate) async fn start_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    warn!("In development, not for production mainnet use");
    crate::fd_limit::raise_for(config.grpc.max_connections)?;
    match config.validator_client {
        ValidatorClient::LegacyRpc => start_over(config, crate::chainview::legacy_client).await,
        ValidatorClient::Grpc => start_over(config, crate::chainview::grpc_client).await,
    }
}

async fn start_over<S: ChainDataSource>(
    config: DaemonConfig,
    client: fn(&TrustedValidatorConfig) -> Result<S, IndexerError>,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let serving = listen(&config, &cancel, &mut tasks).await?;
    #[cfg(feature = "snapshot")]
    if let Some(snapshot) = &config.snapshot {
        crate::bootstrap::bootstrap(snapshot, &config).instrument(component("Snapshot")).await?;
    }
    boot_over(config, client, serving, cancel, tasks).await
}

/// Bound gRPC server's handles: where the pipeline's routes go, its load for `/statusz`
struct Serving<S: ChainDataSource> {
    routing: Routing<S, DiskView>,
    load: Arc<Load>,
}

/// gRPC bound + serving: health from here on, every method `UNAVAILABLE` until the pipeline
/// routes it (EADDRINUSE = boot failure)
async fn listen<S: ChainDataSource>(
    config: &DaemonConfig,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) -> Result<Serving<S>, IndexerError> {
    let limits = GrpcLimits::from(&config.grpc);
    let mut server = GrpcService::new(config.serve.grpc_listen_address, limits)
        .with_trusted_proxies(TrustedProxies::new(config.grpc.trusted_proxies.clone()))
        .with_health(Arc::new(crate::status::PublicHealth));
    if let Some(tls) = &config.serve.tls {
        let files = TlsFiles { cert_path: tls.cert_path.clone(), key_path: tls.key_path.clone() };
        server = server.with_tls(Tls::load(files)?);
    }
    let serving = Serving { routing: server.routing(), load: server.load() };
    crate::hold::serving(server.retire());
    let server = server.bind().await?;
    let span = component("Grpc");
    let (endpoint, network) = (config.serve.grpc_listen_address, network_name(config.network));
    span.in_scope(|| info!(%endpoint, network, "Listening"));
    spawn_infallible(tasks, "grpc", span, server.run(cancel.child_token()));
    Ok(serving)
}

/// Mainnet/testnet: compiled in (no validator needed to boot); regtest: its validator's
async fn upgrade_schedule<S: ChainDataSource>(
    network: NetworkType,
    balancer: &TrafficBalancer<S>,
    validators: usize,
) -> Result<LocalNetwork, IndexerError> {
    if let Some(schedule) = compiled_schedule(network) {
        return Ok(schedule);
    }
    let listed = regtest_schedule(balancer, validators).await;
    listed.schedule().ok_or_else(|| {
        let listed = format!("{:?}", listed.activations);
        IndexerError::Config(format!(
            "regtest validator lists upgrades out of protocol order: {listed}"
        ))
    })
}

/// First poll any trusted validator answers (catching up counts: its schedule is the chain's)
async fn regtest_schedule<S: ChainDataSource>(
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
                Some(Ok(reading)) => {
                    crate::status::awaiting_validator(false);
                    return reading.info.clone();
                }
                Some(Err(cause)) if !warned => {
                    warn!(%cause, "No trusted validator answering yet, waiting to read the upgrade schedule");
                    crate::status::awaiting_validator(true);
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

/// Chain view over the trusted validators (one `client` each) → pipeline over it, routed into
/// `serving` → every task spawned beside the gRPC server's
///
/// - One balancer over every validator; its driver first (regtest's upgrade schedule is a poll's)
async fn boot_over<S: ChainDataSource>(
    config: DaemonConfig,
    client: fn(&TrustedValidatorConfig) -> Result<S, IndexerError>,
    serving: Serving<S>,
    cancel: CancellationToken,
    mut tasks: JoinSet<TaskExit>,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    let started = std::time::Instant::now();
    let chainview_span = component("ChainView");
    let wiring = chainview_span.in_scope(|| crate::chainview::connect(&config, client))?;
    let balancing = wiring.balancing.run(cancel.child_token());
    spawn_infallible(&mut tasks, "traffic", component("Traffic"), balancing);
    let validators = config.trusted_validators.len();
    let schedule = upgrade_schedule(config.network, &wiring.balancer, validators);
    let schedule = schedule.instrument(chainview_span.clone()).await?;
    let viewed = chainview_span.in_scope(|| {
        crate::chainview::view(
            &config,
            &wiring.balancer,
            wiring.p2p,
            schedule,
            cancel.child_token(),
        )
    })?;
    let inputs = Inputs {
        fs: RealFs::shared(),
        started,
        chain: viewed.header_sync.subscribe(),
        view: Arc::clone(&viewed.view),
        balancer: wiring.balancer.clone(),
        schedule,
    };
    let mut tasks = pipeline(&config, inputs, serving, &cancel, tasks)?;

    let header_sync = match config.p2p.enabled {
        true => viewed.header_sync.with_peers(Params::for_network(config.network, &schedule)),
        false => viewed.header_sync,
    };
    let run = header_sync.run(cancel.child_token());
    spawn_infallible(&mut tasks, "header-sync", chainview_span.clone(), run);
    if let Some((starting, watch)) = viewed.peers {
        // one task, ended only by cancel (a task ending = a fault): the start finishes early
        let token = cancel.child_token();
        let run = async move {
            tokio::join!(token.run_until_cancelled(starting), watch.run(token.clone()));
        };
        spawn_infallible(&mut tasks, "peers", chainview_span.clone(), run);
    }
    let fold = viewed.fold.run(cancel.child_token());
    spawn_infallible(&mut tasks, "chainview", chainview_span.clone(), fold);
    for (member, watch) in wiring.watches {
        let (changed, linked) = (wiring.balancer.clone(), wiring.balancer.clone());
        let on_change = move |_| changed.pushed(member, Push::Changed);
        let on_link = move |up| linked.pushed(member, Push::Link(up));
        let run = watch.run(cancel.child_token(), on_change, on_link);
        spawn_infallible(&mut tasks, "push-stream", chainview_span.clone(), run);
    }
    let heartbeat = crate::admin::beat(cancel.child_token());
    spawn(&mut tasks, "heartbeat", component("Metrics"), heartbeat);

    Ok(tokio::spawn(supervise(tasks, cancel, shutdown_signals())))
}

/// Pipeline inputs (production: header sync + trusted validators on disk; tests: mocks on `SimFs`)
struct Inputs<S: ChainDataSource> {
    fs: Arc<dyn Fs>,
    started: std::time::Instant,
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    view: Arc<ChainView<S>>,
    balancer: TrafficBalancer<S>,
    schedule: LocalNetwork,
}

/// The NFS, every enabled index's writer, the global snapshot's publisher, the gRPC routes:
/// [`open_indexes`], then [`run_pipeline`] (an `Err` = nothing of it spawned)
fn pipeline<S: ChainDataSource>(
    config: &DaemonConfig,
    inputs: Inputs<S>,
    serving: Serving<S>,
    cancel: &CancellationToken,
    tasks: JoinSet<TaskExit>,
) -> Result<JoinSet<TaskExit>, IndexerError> {
    let indexes = open_indexes(config, &inputs)?;
    Ok(run_pipeline(config, indexes, inputs, serving, cancel, tasks))
}

/// Every enabled index opened, subscribed to the final stream, added to the NFS
fn open_indexes<S: ChainDataSource>(
    config: &DaemonConfig,
    inputs: &Inputs<S>,
) -> Result<Indexes<S>, IndexerError> {
    let (network, lookahead) = (config.network, config.sync.concurrency);
    let depth = ReorgDepth::new(config.sync.finalised_depth);
    let params = ChainParams { network, schedule: inputs.schedule };
    let (chain, balancer) = (&inputs.chain, &inputs.balancer);
    let mut indexes = Indexes {
        follower: FinalFollower::new(chain.clone(), balancer.clone(), lookahead),
        nfs: Nfs::new(chain.clone(), balancer.clone(), params, depth, lookahead),
        engine: DiskEngine::new(Arc::clone(&inputs.fs), LsmConfig::from(&config.sync.lsm_store)),
        network,
        opened: Vec::new(),
        writers: Vec::new(),
    };
    if let Some((cb, vb)) = config.compact_block()? {
        // compact-block folds after value-balance (its fees)
        let mut fee_sink = FeeSink::new();
        let fees = fee_sink.subscribe(IndexKind::CompactBlock.name(), cb.queue_bytes);
        let (vb_new, vb_handle) = (ValueBalanceIndexWriter::new, ValueBalanceIndexWriter::handle);
        indexes.open(IndexKind::ValueBalance, &vb, vb_new, vb_handle, |writer, blocks| {
            writer.run(blocks, fee_sink)
        })?;
        let (cb_new, cb_handle) = (CompactBlockIndexWriter::new, CompactBlockIndexWriter::handle);
        indexes.open(IndexKind::CompactBlock, &cb, cb_new, cb_handle, |writer, blocks| {
            writer.run(blocks, fees)
        })?;
    }
    if let Some(bh) = config.enabled(IndexKind::BlockHash) {
        indexes.open(
            IndexKind::BlockHash,
            &bh,
            BlockHashIndexWriter::new,
            BlockHashIndexWriter::handle,
            BlockHashIndexWriter::run,
        )?;
    }
    if let Some(ts) = config.enabled(IndexKind::TreeState) {
        indexes.open(
            IndexKind::TreeState,
            &ts,
            TreeStateIndexWriter::new,
            TreeStateIndexWriter::handle,
            TreeStateIndexWriter::run,
        )?;
    }
    if let Some(ta) = config.enabled(IndexKind::TransparentAddress) {
        indexes.open(
            IndexKind::TransparentAddress,
            &ta,
            TransparentAddressIndexWriter::new,
            TransparentAddressIndexWriter::handle,
            TransparentAddressIndexWriter::run,
        )?;
    }
    Ok(indexes)
}

/// Every opened index's writer, the follower, NFS, publisher, progress + egress spawned into
/// `tasks`; gRPC routed; `/statusz` sources published
fn run_pipeline<S: ChainDataSource>(
    config: &DaemonConfig,
    indexes: Indexes<S>,
    inputs: Inputs<S>,
    serving: Serving<S>,
    cancel: &CancellationToken,
    mut tasks: JoinSet<TaskExit>,
) -> JoinSet<TaskExit> {
    let network = config.network;
    let depth = ReorgDepth::new(config.sync.finalised_depth);
    let Indexes { follower, nfs, opened, writers, .. } = indexes;
    for (kind, span, writer) in writers {
        spawn_infallible(&mut tasks, kind.name(), span, writer);
    }
    let publisher = Publisher::new(nfs.indexed(), inputs.view.subscriber(), depth);
    let snapshots = publisher.handle();
    let members = inputs.balancer.members();

    // --- run: every gRPC answer off one snapshot from here on
    let progress = follower.progress();
    spawn(&mut tasks, "follower", component("ZainoSync"), follower.run(cancel.child_token()));
    spawn(&mut tasks, "nfs", component("ZainoNFS"), nfs.run(cancel.child_token()));
    spawn(&mut tasks, "snapshot", component("Snapshot"), publisher.run(cancel.child_token()));
    serving.routing.route(Routes {
        snapshots: snapshots.clone(),
        submit: Arc::clone(&inputs.view),
        validators: inputs.balancer,
        network,
        version: env!("CARGO_PKG_VERSION"),
        max_address_rows: config.serve.max_address_rows,
        indexes: opened.iter().map(|index| index.kind).collect(),
    });
    let utxos = Arc::new(crate::utxos::ServedUtxos(snapshots.clone()));
    if let Some(polling) = inputs.view.peer_mempool(utxos) {
        if config.enabled(IndexKind::CompactBlock).is_none() {
            warn!(
                "p2p.enabled without index.compact_block: no UTXO set, peers admit no transaction"
            );
        }
        let run = polling.run(cancel.child_token());
        spawn_infallible(&mut tasks, "peer-mempool", component("ChainView"), run);
    }
    let (disk, walked) = watch::channel(crate::progress::Disk::new());
    let (rate, block_rate) = watch::channel(None);
    let reporting = crate::progress::run(
        snapshots.clone(),
        progress.clone(),
        opened,
        disk,
        rate,
        cancel.child_token(),
    );
    spawn(&mut tasks, "progress", component("ZainoNFS"), reporting);
    let (egress, egress_windows) = watch::channel(None);
    let measuring = crate::egress::run(egress, cancel.child_token());
    spawn(&mut tasks, "egress", component("Grpc"), measuring);
    crate::status::publish(crate::status::Sources {
        network: network_name(network),
        started: inputs.started,
        snapshots,
        progress,
        members,
        disk: walked,
        block_rate,
        grpc_load: serving.load,
        egress: egress_windows,
    });
    tasks
}

/// Final path + tip overlay + every index opened so far (its writer queued, spawned by
/// [`run_pipeline`])
struct Indexes<S> {
    follower: FinalFollower<S>,
    nfs: Nfs<S, DiskView>,
    engine: DiskEngine,
    network: NetworkType,
    opened: Vec<crate::progress::Index>,
    writers: Vec<(IndexKind, Span, BoxFuture<'static, ()>)>,
}

impl<S: ChainDataSource> Indexes<S> {
    /// `config.path` opened as `kind`'s store (at its crate's write buffer) under its component
    /// span → `writer`'s handle to the follower + NFS, its `run` over the final stream queued
    fn open<W, F>(
        &mut self,
        kind: IndexKind,
        config: &IndexConfig,
        writer: impl FnOnce(DiskStore) -> W,
        handle: impl FnOnce(&W) -> IndexHandle<DiskView>,
        run: impl FnOnce(W, Subscription<Block>) -> F,
    ) -> Result<(), IndexerError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (schema, write_buffer) = stores::schema(kind, self.network);
        let span = crate::logging::index_component(kind.name());
        let writer = span.in_scope(|| {
            debug!("Opening from {}", crate::logging::shown_path(&config.path));
            self.engine.open(&config.path, &schema, write_buffer).map(writer)
        })?;
        let handle = handle(&writer);
        let blocks = self.follower.subscribe(kind, handle.tip(), config.queue_bytes);
        self.nfs.add(kind, handle.clone());
        self.writers.push((kind, span.clone(), Box::pin(run(writer, blocks))));
        self.opened.push(crate::progress::Index { kind, span, dir: config.path.clone(), handle });
        Ok(())
    }
}

/// Signal → `Ok(())`; else the first failure (ending cleanly before shutdown is one)
///
/// - Either way: cancel the rest, then wait for them (writers commit what they hold, gRPC gives
///   open connections `[grpc] shutdown_grace_secs`)
/// - No serving delay on a signal (taking a node out of rotation first = `zainod hold`)
async fn supervise(
    mut tasks: JoinSet<TaskExit>,
    cancel: CancellationToken,
    mut signals: mpsc::Receiver<&'static str>,
) -> Result<(), IndexerError> {
    let mut failure = tokio::select! {
        Some(signal) = signals.recv() => {
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
                failure.get_or_insert(IndexerError::Join(error));
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

/// A task ended before shutdown: always a fault
fn first_failure(exit: Result<TaskExit, tokio::task::JoinError>) -> IndexerError {
    match exit {
        Ok((task, Ok(()))) => IndexerError::TaskEnded { task },
        Ok((task, Err(error))) => {
            error!(task, %error, "Task failed");
            error
        }
        Err(error) => IndexerError::Join(error),
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
            ("fails", Box::pin(async { Err(IndexerError::Config("boom".into())) })),
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
            let outcome = supervise(tasks, cancel.clone(), signals).await;

            let named = match case {
                "ends ok" => matches!(outcome, Err(IndexerError::TaskEnded { task: "early" })),
                "fails" => {
                    matches!(outcome, Err(IndexerError::Config(ref boom)) if boom == "boom")
                }
                _ => {
                    matches!(outcome, Err(IndexerError::Join(ref join)) if join.is_panic())
                }
            };
            assert!(named, "{case}: {outcome:?}");
            assert!(cancel.is_cancelled(), "{case}: rest not cancelled");
            assert!(drained.load(Ordering::SeqCst), "{case}: returned before the drain");
        }
    }

    /// Signal → every task cancelled at once (no serving delay), waited for, clean `Ok`
    #[tokio::test(start_paused = true)]
    async fn a_signal_cancels_every_task_at_once_and_stops_clean() {
        let cancel = CancellationToken::new();
        let stopped = Arc::new(AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        let (token, flag) = (cancel.child_token(), Arc::clone(&stopped));
        spawn(&mut tasks, "serving", Span::none(), async move {
            token.cancelled().await;
            tokio::task::yield_now().await;
            flag.store(true, Ordering::SeqCst);
            Ok::<_, IndexerError>(())
        });
        let (signal, signals) = mpsc::channel(1);
        let started = tokio::time::Instant::now();

        signal.send("SIGTERM").await.expect("supervise receives");
        let outcome = supervise(tasks, cancel.clone(), signals).await;

        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(started.elapsed(), std::time::Duration::ZERO, "a delay before the cancel");
        assert!(cancel.is_cancelled(), "rest not cancelled");
        assert!(stopped.load(Ordering::SeqCst), "returned before the task stopped");
    }

    /// Whole pipeline over a mock validator (indexes on `SimFs`, gRPC on localhost, depth 3):
    /// - A 0..=8 at once (0..=5 final: bulk, committed on idle; 6..=8 folded at the tip)
    /// - then B7 (heavier, off A6) + B8 → reorged heights serve B's, `GetLightdInfo` B8 under
    ///   zainod's version (not zaino-grpc's)
    /// - before + after: `GetLatestBlock`, `GetBlockRange`'s last, `GetTreeState` = one block
    /// - a `GetMempoolStream` opened at A8 ends on the reorg; the `GetLatestBlock` right after it
    ///   is never A8 (G5: the race one global snapshot closes)
    /// - a scrape then renders every ztest family this process emits
    /// - SIGTERM stops every task cleanly (header sync's sender gone mid-drain included)
    #[tokio::test]
    async fn the_pipeline_follows_a_reorg_and_every_rpc_agrees_on_the_served_tip() {
        use std::num::NonZeroU32;
        use std::time::Duration;

        use zaino_header_chain::testing::{insert, HeaderViews};
        use zaino_primitives::testing::{h, MockChain};
        use zaino_primitives::types::Block;
        use zaino_proto::proto::service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
            Empty,
        };
        use zaino_source::testing::ValidatorViews;
        use zcash_protocol::BlockHash;

        // current-thread runtime: every task's samples land here (blocking-pool work's do not)
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let rendered = recorder.handle();
        let _recording = metrics::set_default_local_recorder(&recorder);
        crate::metrics::describe_all();

        let mut blocks = MockChain::regtest().varied_work();
        let a8 = blocks.mine_empty(8);
        let a = blocks.blocks(a8);
        let b8 = blocks.fork(h(6)).outweigh().mine_empty(2).tip();
        let b = blocks.blocks(b8);
        let hash = |block: &Block| block.header().hash;

        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut headers = blocks.header_chain(depth);
        let (verified, chain) = watch::channel(None);
        let validator = Arc::new(blocks.validator(a8));
        let schedule = blocks.schedule().upgrades;
        let limits = zaino_traffic::Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let trusted =
            zaino_traffic::Trusted { source: Arc::clone(&validator), priority: 0, limits };
        let (balancer, balancing) = TrafficBalancer::new(vec![trusted], None);
        let view = ChainView::new(
            vec!["mock".to_owned()],
            balancer.clone(),
            depth,
            schedule,
            tokio_util::sync::CancellationToken::new(),
        );
        let view = Arc::new(view.expect("one endpoint"));
        let fold = view.observation_fold();
        let fs = zaino_persistence::fs::SimFs::new();
        let started = std::time::Instant::now();
        let inputs = Inputs { fs, started, chain, view: Arc::clone(&view), balancer, schedule };
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let address = probe.local_addr().expect("local addr");
        drop(probe);
        let mut config = DaemonConfig {
            network: zcash_protocol::consensus::NetworkType::Regtest,
            ..DaemonConfig::default()
        };
        config.sync.finalised_depth = NonZeroU32::new(3).expect("non-zero");
        config.serve.grpc_listen_address = address;
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let serving = listen(&config, &cancel, &mut tasks).await.expect("the gRPC listener binds");
        spawn_infallible(&mut tasks, "traffic", Span::none(), balancing.run(cancel.child_token()));
        spawn_infallible(&mut tasks, "chainview", Span::none(), fold.run(cancel.child_token()));
        let mut tasks = pipeline(&config, inputs, serving, &cancel, tasks).expect("pipeline up");
        let mut wallet = CompactTxStreamerClient::connect(format!("http://{address}"))
            .await
            .expect("the gRPC listener is bound");
        let mut streamer = wallet.clone();

        // header sync's word, to both its readers (the NFS's watch, the view)
        let mut publish = |added: &[Arc<Block>]| {
            insert(&mut headers, added).expect("valid headers");
            if let Some(boundary) = headers.finalizable() {
                headers.finalize(boundary);
            }
            verified.send_replace(headers.verified().map(Arc::new));
            view.set_verified(headers.verified());
        };
        // (latest, range's last, tree state at it): each the block's (height, hash)
        let mut agree_at = async |tip: &Block| {
            let expected = (u64::from(tip.header().height), tip.header().hash.0);
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
            let ended = loop {
                match stream.message().await {
                    Ok(Some(block)) => last = Some((block.height, block.hash)),
                    Ok(None) => break tonic::Code::Ok,
                    Err(status) => break status.code(),
                }
            };
            assert_eq!(last, Some((expected.0, expected.1.to_vec())), "range ends at the tip");
            assert_eq!(ended, tonic::Code::OutOfRange, "then OUT_OF_RANGE (99 is past it)");
            let state = wallet.get_tree_state(BlockId { height: expected.0, hash: Vec::new() });
            let state = state.await.expect("tree state").into_inner();
            assert_eq!(state.hash, BlockHash(expected.1).to_string(), "tree state's block");
        };

        publish(&a);
        agree_at(&a[8]).await;
        // UNAVAILABLE until the fold's first poll shows the mock holding A8
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut mempool = loop {
            match streamer.get_mempool_stream(Empty {}).await {
                Ok(stream) => break stream.into_inner(),
                Err(unheld) if unheld.code() == tonic::Code::Unavailable => {}
                Err(status) => panic!("GetMempoolStream: {status}"),
            }
            assert!(tokio::time::Instant::now() < deadline, "A8 never held");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        let open = tokio::time::timeout(Duration::from_millis(100), mempool.message()).await;
        assert!(open.is_err(), "served at A8, no arrival: a live, silent stream");
        validator.follow(&blocks, b8);
        publish(&b[7..]);
        let ended = tokio::time::timeout(Duration::from_secs(60), mempool.message()).await;
        let ended = ended.expect("the reorg ends the stream").expect("OK trailers");
        assert!(ended.is_none(), "an empty mempool: no record, only the end");
        let after = streamer.get_latest_block(ChainSpec {}).await.expect("served").into_inner();
        let on_b = b.get(after.height as usize).map(|at| hash(at).0.to_vec());
        assert_eq!(Some(after.hash.clone()), on_b, "{after:?}: on B's chain, never A8");

        agree_at(&b[8]).await;
        let reorged = wallet.get_block(BlockId { height: 7, hash: Vec::new() }).await;
        let reorged = reorged.expect("block 7").into_inner().hash;
        assert_eq!(reorged, hash(&b[7]).0.to_vec(), "7 = B7 now");
        let lightd = wallet.get_lightd_info(Empty {}).await.expect("served").into_inner();
        let identity = (lightd.version.as_str(), lightd.vendor.as_str(), lightd.block_height);
        assert_eq!(identity, (env!("CARGO_PKG_VERSION"), "zaino", 8), "zainod's version at B8");

        let scrape = crate::metrics::scrape(&rendered);
        let per_index = |family: &'static str| {
            let name = move |kind: IndexKind| format!("{family}{{index=\"{}\"}}", kind.name());
            zaino_nfs::INDEXES.into_iter().map(name)
        };
        let families = [
            "zaino_build_info{",
            "zaino_best_tip ",
            "zaino_fetch_height ",
            "zaino_reorgs_total ",
            "zaino_fetch_blocks_total ",
            "zaino_fetch_transactions_total ",
            "zaino_fetch_transparent_inputs_total ",
            "zaino_fetch_transparent_outputs_total ",
            "zaino_fetch_sapling_spends_total ",
            "zaino_fetch_sapling_outputs_total ",
            "zaino_fetch_orchard_actions_total ",
            "zaino_fetch_ironwood_actions_total ",
            "zaino_grpc_first_message_seconds{",
            "zaino_grpc_duration_seconds{",
        ];
        let families = families.into_iter().map(str::to_owned);
        let families = families.chain(per_index("zaino_index_finalized_height"));
        for family in families.chain(per_index("zaino_index_synced")) {
            let found = scrape.lines().any(|line| line.starts_with(&family));
            assert!(found, "ztest family {family:?} not in the scrape:\n{scrape}");
        }

        // header sync's sender drops on the same cancel (NFS + follower see it close mid-drain)
        let token = cancel.child_token();
        spawn_infallible(&mut tasks, "header-sync", Span::none(), async move {
            token.cancelled().await;
            drop(verified);
        });
        let (signal, signals) = mpsc::channel(1);
        signal.send("SIGTERM").await.expect("supervise receives");
        supervise(tasks, cancel, signals).await.expect("SIGTERM = clean stop");
    }
}
