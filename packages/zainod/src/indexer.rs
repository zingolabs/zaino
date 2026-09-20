//! Boots the Zaino daemon on the runtime stack.
//!
//! Translates the daemon [`DaemonConfig`] into the runtime stack's typed params
//! and supervises validator → indexer → store → wallet-gRPC under one
//! [`Orchestra`](zaino_runtime::Orchestra). The stack crates stay config-agnostic;
//! this module is the only place daemon config crosses into them.
//!
//! Scope: serves the index-only compact-block slice
//! (`GetLatestBlock`/`GetBlock`/`GetBlockRange`). Transactions, treestate,
//! address queries, `SendTransaction`, and node JSON-RPC are not served yet.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::{error, info};

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_component::{ComponentName, ReachabilityProbe};
use zaino_indexer::{SourceSyncDriver, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_pre_index_compact_block, index_set};
use zaino_lightserve::{GrpcServer, LightServe};
use zaino_persistence::Namespace;
use zaino_persistence_codec::reserved_namespaces;
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ServeComponent, ValidatorComponent};
use zaino_source::{
    GetChainTip, GetPreIndexCompactBlock, RetryPolicy, SubscribeChainTip, ValidatorClient,
    ValidatorSource,
};
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zaino_store::{StoreComponent, StoreReader};

use crate::config::{DaemonConfig, Network, SourceMode};
use crate::error::IndexerError;

/// Start the Zaino daemon.
///
/// Returns a handle that resolves when the runtime exits: `Ok(())` on a clean
/// shutdown signal or settle, or [`IndexerError::Restart`] on a fatal component
/// escalation (the caller's run loop restarts).
pub async fn start_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    startup_message();
    info!("Starting Zaino");
    spawn_indexer(config).await
}

/// Build the validator source per configured mode, then boot the runtime.
pub async fn spawn_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.validate()?;
    let network = to_zebra_network(config.network);

    match &config.source {
        SourceMode::Direct { zebra_cache_dir } => {
            info!(cache = %zebra_cache_dir.display(), "opening validator ReadState (Direct)");
            let adapter = ZebraReadStateAdapter::open(zebra_cache_dir, &network)
                .map_err(IndexerError::OpenReadState)?;
            boot(adapter, config).await
        }
        // The Rpc selector is preserved in config, but only Direct/ReadState
        // sourcing is wired so far. Fail loud and typed rather than panic.
        SourceMode::Rpc { .. } => Err(IndexerError::RpcSourceUnsupported),
    }
}

/// Boot the runtime over `adapter`: an LMDB-backed compact-block index, the
/// store that composes blocks on read, and the wallet gRPC server, supervised
/// under one Orchestra (validator gated first).
///
/// Generic over the adapter so any source plugs into one boot path (only the
/// ReadState adapter is wired today; the seam is ready for others); the bound is
/// stated on the resilient [`ValidatorClient`] wrapper, which is what the
/// provisioner actually consumes.
async fn boot<A>(
    adapter: A,
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError>
where
    A: ValidatorSource + Send + Sync + 'static,
    ValidatorClient<A>:
        GetPreIndexCompactBlock + GetChainTip + SubscribeChainTip + Send + Sync + 'static,
{
    let source = Arc::new(ValidatorClient::new(adapter, RetryPolicy::default()));

    // LMDB must declare every namespace up front: one per index in the set, plus
    // the engine's reserved watermark / format-version namespaces.
    let namespaces: Vec<Namespace> = index_set()
        .index_ids()
        .into_iter()
        .map(Namespace::from)
        .chain(reserved_namespaces())
        .collect();
    let backend = LmdbBackend::open(LmdbConfig {
        path: config.store.path.clone(),
        map_size_bytes: config.store.map_size_gb << 30,
        namespaces,
    })?;

    // The finalised store: the indexer writes it, the gRPC server composes
    // blocks on read from it. One reader, shared (Arc-backed clone).
    let store_reader = StoreReader::new(Arc::new(backend.clone()));

    // The indexer sources the cheap pre-index compact block and builds the
    // current-zaino index set, resuming from the backend watermark.
    let driver = SourceSyncDriver::resuming_compact(
        &backend,
        index_set(),
        Arc::clone(&source),
        |compact_block| context_from_pre_index_compact_block(&compact_block),
        SyncTuning {
            batch_size: config.indexer.batch_size,
            finalised_depth: config.indexer.finalised_depth,
            channel_capacity: config.indexer.channel_capacity,
            concurrency: config.indexer.concurrency,
        },
    )?;

    // Reachability was already confirmed (Direct opened its state DB), so the
    // runtime's validator gate is a formality here.
    let validator = ValidatorComponent::connect(&AlreadyReachable).await?;
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    let store = StoreComponent::new(ComponentName("store"), store_reader.clone());
    let light_serve = ServeComponent::new(
        ComponentName("light-serve"),
        GrpcServer::new(
            LightServe::new(store_reader),
            config.serve.grpc_listen_address,
        ),
    );

    let mut orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .boot(store)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .boot(light_serve)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .build();

    info!(
        grpc = %config.serve.grpc_listen_address,
        "Zaino runtime booted; serving compact blocks"
    );

    Ok(tokio::spawn(async move {
        // Run until a shutdown signal (clean exit) or a component escalation
        // (fatal → restart). Either way, stop supervising the rest.
        let escalation = tokio::select! {
            signal = shutdown_signal() => {
                info!(signal, "shutdown signal received");
                orchestra.shutdown();
                return Ok(());
            }
            escalation = orchestra.next_escalation() => escalation,
        };
        orchestra.shutdown();
        match escalation {
            Some(component) => {
                error!(%component, "runtime component escalated; restarting");
                Err(IndexerError::Restart)
            }
            None => {
                info!("runtime settled");
                Ok(())
            }
        }
    }))
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

/// Map the daemon's network to zebra's network parameters.
fn to_zebra_network(network: Network) -> zebra_chain::parameters::Network {
    use zebra_chain::parameters::Network as Zebra;
    match network {
        Network::Mainnet => Zebra::Mainnet,
        Network::PubTestnet => Zebra::new_default_testnet(),
        Network::Regtest => Zebra::new_regtest(Default::default()),
    }
}

/// A [`ReachabilityProbe`] that always reports reachable.
///
/// The daemon confirms the validator is reachable before boot (Direct opens its
/// state DB), so the runtime's readiness gate has nothing left to check.
struct AlreadyReachable;

impl ReachabilityProbe for AlreadyReachable {
    async fn reachable(&self) -> bool {
        true
    }
}

/// Prints Zaino's startup banner.
fn startup_message() {
    let welcome_message = r#"
       ░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓░▒▒▒
              Thank you for using ZingoLabs Zaino!

       - Donate to us at https://free2z.cash/zingolabs.

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{welcome_message}");
}
