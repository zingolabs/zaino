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
use std::path::Path;
use std::sync::Arc;

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
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_runtime::{
    IndexerComponent, OrchestraBuilder, RunComponent, ServeComponent, ValidatorComponent,
};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_source_zebra::ZebraValidator;
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zaino_store::{StoreComponent, StoreReader};
use zaino_store_service::Engine;

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

/// Build the validator per configured mode, then boot the runtime.
///
/// Direct mode assembles a [`ZebraValidator`] over both transports: the state
/// database (the finalised-block fast path the indexer and chain-head source
/// through) and JSON-RPC (required for the mempool/passthrough seam, even though
/// the compact-serving slice stubs those). The two are shared behind one `Arc`.
pub async fn spawn_indexer(
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.validate()?;
    let network = to_zebra_network(config.network);

    match &config.source {
        SourceMode::Direct {
            zebra_cache_dir,
            jsonrpc_address,
            cookie_path,
            user,
            password,
        } => {
            info!(cache = %zebra_cache_dir.display(), "opening validator ReadState (Direct)");
            let readstate = ZebraReadStateAdapter::open(zebra_cache_dir, &network)
                .map_err(IndexerError::OpenReadState)?;
            let rpc = ZebraRpcAdapter::new(rpc_client_from_config(
                jsonrpc_address,
                cookie_path.as_deref(),
                user.as_deref(),
                password.as_deref(),
            )?);
            let validator = Arc::new(ZebraValidator::with_read_state(rpc, readstate));
            boot(validator, config).await
        }
        // The Rpc selector is preserved in config, but only Direct/ReadState
        // sourcing is wired so far. Fail loud and typed rather than panic.
        SourceMode::Rpc { .. } => Err(IndexerError::RpcSourceUnsupported),
    }
}

/// Build the validator JSON-RPC client from the configured coordinates.
fn rpc_client_from_config(
    jsonrpc_address: &str,
    cookie_path: Option<&Path>,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<RpcClient, IndexerError> {
    RpcClient::new(RpcClientConfig {
        url: format!("http://{jsonrpc_address}"),
        auth: rpc_auth(cookie_path, user, password)?,
        ..RpcClientConfig::default()
    })
    .map_err(IndexerError::RpcClient)
}

/// The basic-auth credentials the validator expects, from the configured parts.
///
/// A cookie path wins over an explicit user/password pair (a cookie-auth
/// validator rejects the pair); the `__cookie__:` prefix is stripped when
/// present. With neither configured — the regtest default — the client sends no
/// auth.
fn rpc_auth(
    cookie_path: Option<&Path>,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<Option<(String, String)>, IndexerError> {
    match (cookie_path, user, password) {
        (Some(path), _, _) => {
            let contents = std::fs::read_to_string(path).map_err(|source| {
                IndexerError::ConfigError(format!(
                    "reading validator cookie {}: {source}",
                    path.display(),
                ))
            })?;
            let token = contents.trim();
            let token = token.strip_prefix("__cookie__:").unwrap_or(token);
            Ok(Some(("__cookie__".to_string(), token.to_string())))
        }
        (None, Some(user), Some(password)) => Ok(Some((user.to_string(), password.to_string()))),
        (None, _, _) => Ok(None),
    }
}

/// Boot the runtime over the shared `validator`: an LMDB-backed compact-block
/// index (the FS), the self-synchronising non-finalised chain head (the NFS), the
/// engine that composes the two into one served chain, and the wallet gRPC
/// server — all supervised under one Orchestra (validator gated first).
///
/// The one `Arc<ZebraValidator>` backs both source consumers: the FS indexer
/// wraps it in the resilient [`ValidatorClient`]; the chain-head reaches the raw
/// one-shot ports through the `Arc` directly. The chain-head's confirmed-watermark
/// gate is the seam owner — it trims only what the FS has committed.
async fn boot(
    validator: Arc<ZebraValidator>,
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
    // The shared validator handle backs the passthrough controls (broadcast today);
    // the engine reaches it only through the source ports, never the concrete type.
    let engine = Engine::new(
        store_reader.clone(),
        chain_head_subscriber,
        Arc::clone(&validator),
    );

    // Reachability was already confirmed (Direct opened its state DB), so the
    // runtime's validator gate is a formality here.
    let validator_component = ValidatorComponent::connect(&AlreadyReachable).await?;
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
