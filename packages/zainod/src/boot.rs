//! Daemon boot + pipeline composition (`docs/architecture.md` §2)
//!
//! - Only place daemon config crosses into the (config-agnostic) stack crates
//! - Only place the pipeline's shape is written down:
//!
//! ```text
//!   validators ──▶ HeaderSync ── HeaderChainSnapshot ─┬─▶ FinalizedFollower ─▶ finalized stream ─┬─▶ utxo_set ─fees───────┐
//!   validators ──▶ getblock <hash> (checked) ─────────┤                                          ├─▶ compact_block ◀──────┘
//!                                                     │                                          ├─▶ block_hash
//!                                                     │                                          ├─▶ tree_state
//!                                                     │                                          └─▶ transparent_address
//!                                                     └─▶ Nfs (tip overlay) ◀── each writer's IndexHandle ──────────┘
//!                                                          ▼
//!                            NfsPublication ─┐
//!   ChainView (holders, mempool, facts) ─────┴─▶ Publisher ─▶ GrpcSnapshots ─▶ RouteDeps ─▶ GrpcService
//!   ChainView (send) + TrafficBalancer (GetTransaction) ─────────────────────────┘       └▶ admin, logs
//! ```
//!
//! - Finalized path: follower → writers (disk)
//! - Tip path: NFS (RAM)
//! - One `GrpcSnapshot` per request
//! - Each writer = its own task over its subscription (disabled index never opened)
//! - Every stage = one plain task in a `JoinSet` (first to end ends the daemon)

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn, Instrument as _, Span};

use crate::config::network_name;
use zaino_chainview::ChainView;
use zaino_grpc::{AdmissionState, GrpcService, RouteDeps, Routing, Tls};
use zaino_grpc_snapshot::Publisher;
use zaino_header_chain::{HeaderChainSnapshot, Params, CONTEXT};
use zaino_index_tree_state::TreeStateReader;
use zaino_nfs::{write_buffer, writer, ChainParams, FeeWiring, Nfs};
use zaino_persistence::fs::{Fs, RealFs};
use zaino_persistence::{DiskEngine, DiskView, IndexKind, PersistenceEngine, View};
use zaino_primitives::protocol::compiled_schedule;
use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_source::{BlockchainInfo, ChainDataSource};
use zaino_sync::{FinalizedFollower, IndexHandle};
use zaino_traffic::{Push, TrafficBalancer, TrustedValidatorId};
use zcash_protocol::consensus::{BlockHeight, NetworkType};
use zcash_protocol::local_consensus::LocalNetwork;

use crate::config::{DaemonConfig, IndexConfig, TrustedValidatorConfig, ValidatorClient};
use crate::error::IndexerError;
use crate::logging::component;
use crate::status::DaemonStatus;

/// Task name + outcome (name → log line for a task that ends early with `Ok`)
type TaskExit = (&'static str, Result<(), IndexerError>);

/// Handle → `Ok(())` on a shutdown signal, else the first task's failure (process exits, nothing
/// restarts in-process)
///
/// - signal handlers first: a signal anywhere in boot (snapshot install, regtest schedule wait) =
///   clean stop
/// - one client per validator next (unprobed: a bad entry fails before any bind or download)
/// - gRPC listener: health answers through the index-snapshot install, a taken port fails
///   before any download
/// - then the snapshot install, then boot (no validator needs to answer first)
pub(crate) async fn start(
    config: DaemonConfig,
    status: Arc<DaemonStatus>,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    warn!("In development, not for production mainnet use");
    let signals = shutdown_signals();
    crate::fd_limit::raise_for(config.grpc.max_connections)?;
    match config.validator_client {
        ValidatorClient::LegacyRpc => {
            start_over(config, crate::chainview::legacy_client, status, signals).await
        }
        ValidatorClient::Grpc => {
            start_over(config, crate::chainview::grpc_client, status, signals).await
        }
    }
}

async fn start_over<S: ChainDataSource>(
    config: DaemonConfig,
    client: fn(&TrustedValidatorConfig) -> Result<S, IndexerError>,
    status: Arc<DaemonStatus>,
    mut signals: mpsc::Receiver<&'static str>,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    let wiring = component("ChainView").in_scope(|| crate::chainview::connect(&config, client))?;
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let booting = async {
        let serving = listen(&config, &status, &cancel, &mut tasks).await?;
        if let Some(snapshot) = &config.snapshot {
            let install = crate::index_snapshot::install(snapshot, &config, &status);
            install.instrument(component("IndexSnapshot")).await?;
        }
        boot_over(&config, wiring, serving, &status, &cancel, &mut tasks).await
    };
    if let Some(signal) = unless_signalled(booting, &mut signals).await? {
        info!(signal, "Shutdown signal received during boot");
        cancel.cancel();
    }
    Ok(tokio::spawn(supervise(tasks, cancel, signals)))
}

/// `boot` to its end, or a signal first (`Some`: `boot` dropped, an aria2c child killed with it)
async fn unless_signalled(
    boot: impl Future<Output = Result<(), IndexerError>>,
    signals: &mut mpsc::Receiver<&'static str>,
) -> Result<Option<&'static str>, IndexerError> {
    tokio::select! {
        booted = boot => booted.map(|()| None),
        Some(signal) = signals.recv() => Ok(Some(signal)),
    }
}

/// Bound gRPC server's handles: where the pipeline's routes go, its admission for `Overloaded`
struct Serving<S: ChainDataSource> {
    routing: Routing<S, DiskView>,
    admission: Arc<AdmissionState>,
}

/// gRPC bound + serving: health from here on, every method `UNAVAILABLE` until the pipeline
/// routes it (EADDRINUSE = boot failure)
async fn listen<S: ChainDataSource>(
    config: &DaemonConfig,
    status: &Arc<DaemonStatus>,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) -> Result<Serving<S>, IndexerError> {
    let mut server = GrpcService::new(config.serve.grpc_listen_address, config.grpc.clone())
        .with_retire(status.retire().clone())
        .with_health(Arc::clone(status) as Arc<dyn zaino_grpc::Health>);
    if let Some(tls) = &config.serve.tls {
        server = server.with_tls(Tls::load(tls.clone())?);
    }
    let serving = Serving { routing: server.routing(), admission: server.admission() };
    let overload = config.grpc.overload.clone();
    crate::overload::spawn(server.admission(), &config.serve.state_dir, overload);
    let server = server.bind().await?;
    let span = component("Grpc");
    let (endpoint, network) = (config.serve.grpc_listen_address, network_name(config.network));
    span.in_scope(|| info!(%endpoint, network, "Listening"));
    spawn_infallible(tasks, "grpc", span, server.run(cancel.child_token()));
    Ok(serving)
}

/// - Mainnet/testnet: compiled in (no validator needed to boot)
/// - Regtest: its validator's
async fn upgrade_schedule<S: ChainDataSource>(
    network: NetworkType,
    balancer: &TrafficBalancer<S>,
    validators: usize,
    status: &DaemonStatus,
) -> Result<LocalNetwork, IndexerError> {
    if let Some(schedule) = compiled_schedule(network) {
        return Ok(schedule);
    }
    let listed = regtest_schedule(balancer, validators, status).await;
    let schedule = listed.schedule();
    schedule
        .map_err(|refused| IndexerError::Config(format!("regtest validator's schedule: {refused}")))
}

/// First poll any trusted validator answers (catching up counts: its schedule is the chain's)
async fn regtest_schedule<S: ChainDataSource>(
    balancer: &TrafficBalancer<S>,
    validators: usize,
    status: &DaemonStatus,
) -> BlockchainInfo {
    let members = (0..validators).filter_map(TrustedValidatorId::new);
    let mut observed: Vec<_> = members.map(|member| balancer.observe(member)).collect();
    let mut warned = false;
    loop {
        for observation in &mut observed {
            let observation = observation.borrow_and_update().clone();
            match observation.as_ref().map(|observation| &observation.polled) {
                Some(Ok(reading)) => {
                    status.awaiting_validator(false);
                    return reading.info.clone();
                }
                Some(Err(cause)) if !warned => {
                    warn!(%cause, "No trusted validator answering yet, waiting to read the upgrade schedule");
                    status.awaiting_validator(true);
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

/// Chain view over the trusted validators ([`crate::chainview::connect`]'s `wiring`) → pipeline
/// over it, routed into `serving` → every task spawned beside the gRPC server's
///
/// - One balancer over every validator
/// - Balancer's driver first (regtest's upgrade schedule = a poll's)
async fn boot_over<S: ChainDataSource>(
    config: &DaemonConfig,
    wiring: crate::chainview::Wiring<S>,
    serving: Serving<S>,
    status: &Arc<DaemonStatus>,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) -> Result<(), IndexerError> {
    let chainview_span = component("ChainView");
    let balancing = wiring.balancing.run(cancel.child_token());
    spawn_infallible(tasks, "traffic", component("Traffic"), balancing);
    let validators = config.trusted_validators.len();
    let schedule = upgrade_schedule(config.network, &wiring.balancer, validators, status);
    let schedule = schedule.instrument(chainview_span.clone()).await?;
    let viewed = chainview_span.in_scope(|| {
        crate::chainview::view(config, &wiring.balancer, wiring.p2p, schedule, cancel.child_token())
    });
    let inputs = Inputs {
        fs: RealFs::shared(),
        chain: viewed.header_sync.subscribe(),
        view: Arc::clone(&viewed.view),
        balancer: wiring.balancer.clone(),
        schedule,
    };
    let own = pipeline(config, inputs, serving, status, cancel, tasks)?;

    let header_sync = match config.p2p.enabled {
        true => viewed.header_sync.with_peers(Params::for_network(config.network, &schedule)),
        false => viewed.header_sync,
    };
    let header_sync = match own {
        Some(own) => header_sync.with_own_chain(own),
        None => header_sync,
    };
    let run = header_sync.run(cancel.child_token());
    spawn_infallible(tasks, "header-sync", chainview_span.clone(), run);
    if let Some((starting, watch)) = viewed.peers {
        // one task, ended only by cancel (a task ending = a fault): the start finishes early
        let token = cancel.child_token();
        let run = async move {
            tokio::join!(token.run_until_cancelled(starting), watch.run(token.clone()));
        };
        spawn_infallible(tasks, "peers", chainview_span.clone(), run);
    }
    let merge = viewed.merge.run(cancel.child_token());
    spawn_infallible(tasks, "chainview", chainview_span.clone(), merge);
    for (member, watch) in wiring.watches {
        let (changed, linked) = (wiring.balancer.clone(), wiring.balancer.clone());
        let on_change = move || changed.pushed(member, Push::Changed);
        let on_link = move |up| linked.pushed(member, Push::Link(up));
        let run = watch.run(cancel.child_token(), on_change, on_link);
        spawn_infallible(tasks, "push-stream", chainview_span.clone(), run);
    }
    let heartbeat = crate::admin::beat(Arc::clone(status), cancel.child_token());
    spawn(tasks, "heartbeat", component("Admin"), heartbeat);
    Ok(())
}

/// Pipeline inputs (production: header sync + trusted validators on disk, tests: mocks on `SimFs`)
struct Inputs<S: ChainDataSource> {
    fs: Arc<dyn Fs>,
    chain: watch::Receiver<Option<Arc<HeaderChainSnapshot>>>,
    view: Arc<ChainView<S>>,
    balancer: TrafficBalancer<S>,
    schedule: LocalNetwork,
}

/// The NFS, every enabled index's writer, the `GrpcSnapshot` publisher, the gRPC routes:
/// [`open_indexes`], then [`run_pipeline`] (an `Err` = nothing of it spawned) → [`own_chain`]
fn pipeline<S: ChainDataSource>(
    config: &DaemonConfig,
    inputs: Inputs<S>,
    serving: Serving<S>,
    status: &DaemonStatus,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) -> Result<Option<Vec<BlockRef>>, IndexerError> {
    let indexes = open_indexes(config, &inputs)?;
    let own = own_chain(&indexes.opened);
    run_pipeline(config, indexes, inputs, serving, status, cancel, tasks);
    Ok(own)
}

/// The indexes' chain at their lowest durable tip, by hash: `CONTEXT` + 2 blocks from
/// tree-state's records (`None` = tree-state off, or fewer blocks held)
fn own_chain(opened: &[OpenedIndex]) -> Option<Vec<BlockRef>> {
    let tips = opened.iter().map(|index| index.handle.durable_view().tip());
    let tip = tips.min_by_key(|tip| tip.map(|tip| tip.height))??;
    let tree_state = opened.iter().find(|index| index.kind == IndexKind::TreeState)?;
    let reader = TreeStateReader::new(tree_state.handle.durable_view());
    let from = u32::from(tip.height).checked_sub(CONTEXT as u32 + 1)?;
    let at = |height: u32| {
        let height = BlockHeight::from(height);
        reader.treestate(height).ok().map(|state| BlockRef { hash: state.block_hash, height })
    };
    (from..=u32::from(tip.height)).map(at).collect()
}

/// Every enabled index opened, subscribed to the finalized stream, added to the NFS
fn open_indexes<S: ChainDataSource>(
    config: &DaemonConfig,
    inputs: &Inputs<S>,
) -> Result<Indexes<S>, IndexerError> {
    let (network, lookahead) = (config.network, config.sync.concurrency);
    let depth = FinalityDepth::new(config.sync.finalised_depth);
    let params = ChainParams { network, schedule: inputs.schedule };
    let (chain, balancer) = (&inputs.chain, &inputs.balancer);
    let mut indexes = Indexes {
        follower: FinalizedFollower::new(chain.clone(), balancer.clone(), lookahead),
        nfs: Nfs::new(chain.clone(), balancer.clone(), params, depth, lookahead),
        engine: DiskEngine::new(Arc::clone(&inputs.fs), config.sync.lsm_store),
        network,
        opened: Vec::new(),
        writers: Vec::new(),
    };
    let compact_block = config.compact_block()?;
    let mut fees = FeeWiring::new(compact_block.map(|(cb, _)| cb.queue_bytes));
    for kind in IndexKind::ALL {
        if let Some(index) = config.enabled(kind) {
            indexes.open(kind, &index, &mut fees)?;
        }
    }
    Ok(indexes)
}

/// - Every opened index's writer, the follower, NFS, publisher, progress + egress → `tasks`
/// - gRPC routed, `status` sources published
fn run_pipeline<S: ChainDataSource>(
    config: &DaemonConfig,
    indexes: Indexes<S>,
    inputs: Inputs<S>,
    serving: Serving<S>,
    status: &DaemonStatus,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<TaskExit>,
) {
    let network = config.network;
    let depth = FinalityDepth::new(config.sync.finalised_depth);
    let Indexes { follower, nfs, opened, writers, .. } = indexes;
    for (kind, span, writer) in writers {
        spawn_infallible(tasks, kind.name(), span, writer);
    }
    let publisher = Publisher::new(nfs.indexed(), inputs.view.subscriber(), depth);
    let grpc_snapshots = publisher.handle();

    // --- run: every gRPC answer off one `GrpcSnapshot` from here on
    let progress = follower.progress();
    spawn(tasks, "follower", component("ZainoSync"), follower.run(cancel.child_token()));
    spawn(tasks, "nfs", component("ZainoNFS"), nfs.run(cancel.child_token()));
    spawn(tasks, "snapshot", component("GrpcSnapshot"), publisher.run(cancel.child_token()));
    serving.routing.route(RouteDeps {
        grpc_snapshots: grpc_snapshots.clone(),
        chain_view: Arc::clone(&inputs.view),
        validators: inputs.balancer,
        network,
        version: env!("CARGO_PKG_VERSION"),
        max_address_rows: config.serve.max_address_rows,
        indexes: opened.iter().map(|index| index.kind).collect(),
    });
    let utxos = Arc::new(crate::utxos::ServedUtxos(grpc_snapshots.clone()));
    if let Some(polling) = inputs.view.peer_mempool(utxos) {
        if config.enabled(IndexKind::CompactBlock).is_none() {
            warn!(
                "p2p.enabled without index.compact_block: no UTXO set, peers admit no transaction"
            );
        }
        let run = polling.run(cancel.child_token());
        spawn_infallible(tasks, "peer-mempool", component("ChainView"), run);
    }
    let (rate, block_rate) = watch::channel(None);
    let reporting = crate::progress::run(
        grpc_snapshots.clone(),
        progress.clone(),
        opened,
        rate,
        cancel.child_token(),
    );
    spawn(tasks, "progress", component("ZainoNFS"), reporting);
    let (egress, egress_windows) = watch::channel(None);
    let measuring = crate::egress::run(egress, cancel.child_token());
    spawn(tasks, "egress", component("Grpc"), measuring);
    status.publish(crate::status::Sources {
        grpc_snapshots,
        progress,
        block_rate,
        grpc_admission: serving.admission,
        egress: egress_windows,
    });
}

/// Finalized path + tip overlay + every index opened so far (its writer queued, spawned by
/// [`run_pipeline`])
struct Indexes<S> {
    follower: FinalizedFollower<S>,
    nfs: Nfs<S, DiskView>,
    engine: DiskEngine,
    network: NetworkType,
    opened: Vec<OpenedIndex>,
    writers: Vec<(IndexKind, Span, BoxFuture<'static, ()>)>,
}

/// One enabled index: its log component, directory and writer's handle
pub(crate) struct OpenedIndex {
    pub(crate) kind: IndexKind,
    pub(crate) span: Span,
    pub(crate) dir: PathBuf,
    pub(crate) handle: IndexHandle<DiskView>,
}

impl<S: ChainDataSource> Indexes<S> {
    /// `config.path` opened as `kind`'s store under its component span → its writer's handle to
    /// the follower + NFS, its `run` over the finalized stream queued
    fn open(
        &mut self,
        kind: IndexKind,
        config: &IndexConfig,
        fees: &mut FeeWiring,
    ) -> Result<(), IndexerError> {
        let (schema, write_buffer) = (zaino_nfs::schema(kind, self.network), write_buffer(kind));
        let span = crate::logging::index_component(kind.name());
        let store = span.in_scope(|| {
            debug!("Opening from {}", crate::logging::shown_path(&config.path));
            self.engine.open(&config.path, &schema, write_buffer)
        })?;
        let (handle, run) = writer(kind, store, fees);
        let blocks = self.follower.subscribe(kind, handle.durable_view().tip(), config.queue_bytes);
        self.nfs.add(kind, handle.clone());
        self.writers.push((kind, span.clone(), run(blocks)));
        self.opened.push(OpenedIndex { kind, span, dir: config.path.clone(), handle });
        Ok(())
    }
}

/// Signal → `Ok(())`, else the first failure (ending cleanly before shutdown = one)
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
        // in order: a signal, a cancel set during boot (its tasks' ends = no failure), a task's end
        biased;
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

    /// Real SIGTERM to this process while boot waits on a regtest validator that never answers:
    /// - boot returns (no download, no schedule needed), the daemon's handle ends `Ok`
    /// - the gRPC listener bound during boot is closed by then
    #[tokio::test]
    async fn a_shutdown_signal_during_boot_stops_the_daemon_cleanly() {
        use std::num::NonZeroUsize;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let (grpc, validator) = (probe.local_addr().expect("local addr"), "127.0.0.1:1");
        drop(probe);
        let mut config = DaemonConfig {
            network: NetworkType::Regtest,
            trusted_validators: vec![TrustedValidatorConfig {
                jsonrpc_address: validator.to_owned(),
                ..TrustedValidatorConfig::default()
            }],
            ..DaemonConfig::default()
        };
        config.serve.grpc_listen_address = grpc;
        config.serve.state_dir = dir.path().join("state");
        config.grpc.max_connections = NonZeroUsize::new(16).expect("non-zero");
        let status = Arc::new(DaemonStatus::new(config.network, []));
        let booting = tokio::spawn(start(config, Arc::clone(&status)));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !status.readiness_json().1.contains("No trusted validator reachable") {
            assert!(tokio::time::Instant::now() < deadline, "boot never waited on its validator");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(std::net::TcpStream::connect(grpc).is_ok(), "gRPC bound during boot");
        let this = rustix::process::getpid();
        rustix::process::kill_process(this, rustix::process::Signal::TERM).expect("SIGTERM sent");

        let booted = tokio::time::timeout(Duration::from_secs(10), booting).await;
        let running = booted.expect("boot returns on the signal").expect("joins");
        let stopped = running.expect("no boot failure").await.expect("joins");
        assert!(stopped.is_ok(), "SIGTERM during boot = clean stop: {stopped:?}");
        assert!(std::net::TcpStream::connect(grpc).is_err(), "listener closed");
    }

    /// `boot` raced against the signal channel
    ///
    /// - Boot first = its outcome
    /// - Signal first = boot dropped mid-await (its holdings released: aria2c child killed on drop)
    #[tokio::test]
    async fn boot_raced_against_a_signal_drops_boot_on_the_signal() {
        let (signal, mut signals) = mpsc::channel(1);
        let done = unless_signalled(async { Ok(()) }, &mut signals).await;
        assert!(matches!(done, Ok(None)), "{done:?}");
        let failed =
            unless_signalled(async { Err(IndexerError::Config("x".into())) }, &mut signals);
        assert!(matches!(failed.await, Err(IndexerError::Config(_))));

        struct Held(Arc<AtomicBool>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let held = Held(Arc::clone(&released));
        let pending = async move {
            let _held = held;
            std::future::pending::<Result<(), IndexerError>>().await
        };
        signal.send("SIGINT").await.expect("receiver open");
        let signalled = unless_signalled(pending, &mut signals).await;
        assert!(matches!(signalled, Ok(Some("SIGINT"))), "{signalled:?}");
        assert!(released.load(Ordering::SeqCst), "boot dropped");
    }

    /// Whole pipeline over a mock validator (indexes on `SimFs`, gRPC on localhost, depth 3)
    ///
    /// - A 0..=8 at once (0..=5 finalized: bulk, committed on idle)
    /// - A 6..=8 folded at the tip
    /// - then B7 (heavier, off A6) + B8 → reorged heights serve B's, `GetLightdInfo` B8 under
    ///   zainod's version (not zaino-grpc's)
    /// - before + after: `GetLatestBlock`, `GetBlockRange`'s last, `GetTreeState` = one block
    /// - a `GetMempoolStream` opened at A8 ends on the reorg
    /// - `GetLatestBlock` right after it never A8 (G5: the race one `GrpcSnapshot` closes)
    /// - a scrape then renders every ztest family this process emits
    /// - SIGTERM stops every task cleanly (header sync's sender gone mid-drain included)
    #[tokio::test]
    async fn the_pipeline_follows_a_reorg_and_every_rpc_agrees_on_the_served_tip() {
        use std::num::NonZeroU32;
        use std::time::Duration;

        use zaino_header_chain::testing::insert;
        use zaino_header_chain::HeaderChain;
        use zaino_primitives::types::Block;
        use zaino_proto::proto::service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
            Empty,
        };
        use zaino_source::testing::{h, MockChain};
        use zcash_protocol::BlockHash;

        // current-thread runtime: every task's samples land here (blocking-pool work's do not)
        let (recorder, rendered) = crate::metrics::recorder().expect("native config is valid");
        let _recording = metrics::set_default_local_recorder(&recorder);
        crate::metrics::describe_all();

        let mut blocks = MockChain::regtest().varied_work();
        let a8 = blocks.mine_empty(8);
        let a = blocks.blocks(a8);
        let b8 = blocks.fork(h(6)).outweigh().mine_empty(2).tip();
        let b = blocks.blocks(b8);
        let hash = |block: &Block| block.header().hash;

        let depth = FinalityDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut headers = HeaderChain::anchored(&blocks, depth);
        let (verified, chain) = watch::channel(None);
        let validator = blocks.validator(a8);
        let schedule = blocks.schedule().upgrades;
        let limits = zaino_traffic::Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
        let trusted = zaino_traffic::TrustedValidator {
            address: "mock".to_owned(),
            source: Arc::new(validator.client()),
            priority: 0,
            limits,
        };
        let (balancer, balancing) = TrafficBalancer::new(vec![trusted], None);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let view = Arc::new(ChainView::new(balancer.clone(), depth, schedule, shutdown));
        let merge = view.observation_merge();
        let fs = zaino_persistence::fs::SimFs::new();
        let inputs = Inputs { fs, chain, view: Arc::clone(&view), balancer, schedule };
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
        let status = Arc::new(DaemonStatus::new(config.network, []));
        let serving =
            listen(&config, &status, &cancel, &mut tasks).await.expect("the gRPC listener binds");
        spawn_infallible(&mut tasks, "traffic", Span::none(), balancing.run(cancel.child_token()));
        spawn_infallible(&mut tasks, "chainview", Span::none(), merge.run(cancel.child_token()));
        pipeline(&config, inputs, serving, &status, &cancel, &mut tasks).expect("pipeline up");
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
            verified.send_replace(headers.snapshot().map(Arc::new));
            view.set_chain(headers.snapshot());
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
        // UNAVAILABLE until the merge's first poll shows the mock holding A8
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

        use crate::metrics::{scrape, Format};
        let text = String::from_utf8(scrape(&rendered, &status, Format::Text));
        let text = text.expect("text exposition is UTF-8");
        let per_index = |family: &'static str| {
            let name = move |kind: IndexKind| format!("{family}{{index=\"{}\"}}", kind.name());
            IndexKind::ALL.iter().copied().map(name)
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
        ];
        let families = families.into_iter().map(str::to_owned);
        let families = families.chain(per_index("zaino_index_finalized_height"));
        for family in families.chain(per_index("zaino_index_synced")) {
            let found = text.lines().any(|line| line.starts_with(&family));
            assert!(found, "ztest family {family:?} not in the text scrape:\n{text}");
        }
        // native histograms: protobuf scrape only (lossy: names are plain strings in the payload)
        let protobuf = scrape(&rendered, &status, Format::Protobuf);
        let protobuf = String::from_utf8_lossy(&protobuf);
        for family in ["zaino_grpc_first_message_seconds", "zaino_grpc_duration_seconds"] {
            assert!(
                protobuf.contains(family),
                "ztest histogram {family:?} not in the protobuf scrape"
            );
            assert!(!text.contains(family), "{family:?} in the text scrape = not native");
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
