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

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{error, info};

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_head_service::ChainHeadService;
use zaino_component::{CancellationToken, ComponentName, ReachabilityProbe};
use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;
use zaino_indexer::{SourceSyncDriver, SyncTuning};
use zaino_indexes::sets::current_zaino::{context_from_pre_index_compact_block, index_set};
use zaino_lightserve::{GrpcServer, LightServe};
use zaino_persistence::Namespace;
use zaino_persistence_codec::reserved_namespaces;
use zaino_runtime::{
    IndexerComponent, OrchestraBuilder, RunComponent, ServeComponent, ValidatorComponent,
};
use zaino_source::{OneShotGetChainTip, RetryPolicy, ValidatorClient};
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zaino_store::{StoreComponent, StoreReader};
use zaino_store_service::Engine;

use crate::config::{DaemonConfig, SourceConfig};
use crate::error::IndexerError;

/// Tip poll cadence for the FS indexer's follow loop (= chain-head default poll)
const TIP_POLL_INTERVAL: Duration = Duration::from_secs(1);

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

/// Wait for the validator's JSON-RPC to answer, build the source over it, then boot the runtime.
pub async fn spawn_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    let validator = Arc::new(connect_validator(&config.source).await?);
    boot(validator, config).await
}

/// JSON-RPC source with a live tip subscription
async fn connect_validator(source: &SourceConfig) -> Result<ZebraRpcAdapter, IndexerError> {
    let adapter = ZebraRpcAdapter::connect(
        &source.jsonrpc_address,
        source.cookie_path.as_deref(),
        source.user.clone(),
        source.password.clone(),
    )
    .await?;
    info!(validator = %source.jsonrpc_address, "validator JSON-RPC answering");
    adapter
        .with_tip_polling(TIP_POLL_INTERVAL)
        .await
        .map_err(IndexerError::TipPolling)
}

/// Boot the runtime over the shared `validator`: an LMDB-backed compact-block
/// index (the FS), the self-synchronising non-finalised chain head (the NFS), the
/// engine that composes the two into one served chain, and the wallet gRPC
/// server — all supervised under one Orchestra (validator gated first).
///
/// The one `Arc<ZebraRpcAdapter>` backs both source consumers: the FS indexer
/// wraps it in the resilient [`ValidatorClient`]; the chain-head reaches the raw
/// one-shot ports through the `Arc` directly. The chain-head's confirmed-watermark
/// gate is the seam owner — it trims only what the FS has committed.
async fn boot(
    validator: Arc<ZebraRpcAdapter>,
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    // The FS indexer sources through the resilient wrapper over the shared
    // validator; the chain-head reaches the same validator's raw one-shot ports
    // through the `Arc`.
    let source = Arc::new(ValidatorClient::new(
        Arc::clone(&validator),
        RetryPolicy::default(),
    ));

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

    // The finalised store: the indexer writes it, the engine composes blocks on
    // read from it. One reader, shared (Arc-backed clone).
    let store_reader = StoreReader::new(Arc::new(backend.clone()));

    // The FS indexer sources the cheap pre-index compact block and builds the
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
    // Capture the confirmed-watermark receiver before the driver is moved into
    // its component — it is the chain-head's only handle onto what the FS has
    // durably committed (confirm-before-trim).
    let confirmed_watermark = driver.subscribe_confirmed_watermark();

    // The NFS chain head, anchored over the raw validator. Its cancel is a child
    // of the runtime's root token (governs anchoring; the run loop is cancelled
    // through the token its RunComponent hands it).
    let runtime_cancel = CancellationToken::new();
    let (chain_head_subscriber, chain_head_writer) = ChainHeadService::anchor(
        Arc::clone(&validator),
        ChainHeadConfig::with_max_depth(
            NonZeroU32::new(MAX_BLOCK_REORG_HEIGHT).expect("the consensus reorg bound is non-zero"),
        ),
        confirmed_watermark,
        runtime_cancel.child_token(),
    )
    .await
    .map_err(IndexerError::ChainHeadInit)?;

    // Compose FS ⊕ NFS into the served engine, behind the light-wallet profile.
    let engine = Engine::new(store_reader.clone(), chain_head_subscriber);

    let validator_component = ValidatorComponent::connect(&TipReachable(&validator)).await?;
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    let store = StoreComponent::new(ComponentName("store"), store_reader);
    // The chain-head writer is escalated and supervised exactly like the indexer.
    let chain_head = RunComponent::new(ComponentName("chain-head"), chain_head_writer);
    let light_serve = ServeComponent::new(
        ComponentName("light-serve"),
        GrpcServer::new(LightServe::new(engine), config.serve.grpc_listen_address),
    );

    // Readiness-gated order: validator, then the FS indexer (so its watermark is
    // published before the chain-head trims against it), then the store, then the
    // chain-head writer, then the server.
    let mut orchestra = OrchestraBuilder::new()
        .boot_observed(validator_component)
        .await
        .boot(indexer)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .boot(store)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .boot(chain_head)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .boot(light_serve)
        .await
        .map_err(|e| IndexerError::Boot(Box::new(e)))?
        .build();

    info!(
        grpc = %config.serve.grpc_listen_address,
        "Zaino runtime booted; serving compact blocks over the composed FS⊕NFS chain"
    );

    Ok(tokio::spawn(async move {
        // Run until a shutdown signal (clean exit) or a component escalation
        // (fatal → restart). Either way, stop supervising the rest.
        let escalation = tokio::select! {
            signal = shutdown_signal() => {
                info!(signal, "shutdown signal received");
                orchestra.shutdown();
                runtime_cancel.cancel();
                return Ok(());
            }
            escalation = orchestra.next_escalation() => escalation,
        };
        orchestra.shutdown();
        runtime_cancel.cancel();
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

/// Validator gate: reachable = answers a tip read
struct TipReachable<'a>(&'a ZebraRpcAdapter);

impl ReachabilityProbe for TipReachable<'_> {
    async fn reachable(&self) -> bool {
        self.0.get_chain_tip().await.is_ok()
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
