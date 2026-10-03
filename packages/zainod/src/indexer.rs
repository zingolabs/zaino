//! Boots the Zaino daemon on the runtime stack.
//!
//! Translates the daemon [`DaemonConfig`] into the runtime's typed params:
//! builds the one validator client every consumer shares, selects the
//! deployment config names, and hands both to the runtime's assembly
//! ([`zaino_runtime::boot_indexed`]) together with the serving adapter that
//! speaks the deployment's use case. This module is the only place daemon
//! config crosses into the stack, and the only place a runtime value becomes
//! a type.

use std::path::Path;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::{error, info};

use zaino_lightserve::{GrpcServer, LightServe};
use zaino_noderpc::{JsonRpcServer, NodeRpc};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_runtime::config::IndexedDeploymentConfig;
use zaino_runtime::deployment::{
    LightWalletLocal, LightWalletPassthrough, LightWalletPassthroughSource, NodeRpcLocal,
    NodeRpcSource,
};
use zaino_runtime::{boot_indexed, Orchestra};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_source_zebra::ZebraValidator;
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zcash_protocol::consensus::Network as ZcashNetwork;

use crate::config::{DaemonConfig, DeploymentKind, Network, SourceMode};
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
            let validator = ZebraValidator::with_read_state(rpc, readstate)
                .with_tip_polling(
                    tip_probe(
                        jsonrpc_address,
                        cookie_path.as_deref(),
                        user.as_deref(),
                        password.as_deref(),
                    )?,
                    TIP_POLL_INTERVAL,
                )
                .await
                .map_err(IndexerError::TipPolling)?;
            select_deployment(client_over(Arc::new(validator)), config).await
        }
        // Off-node: reach the validator over JSON-RPC alone, no co-located state
        // DB. The FS indexer sources compact blocks over RPC and the chain-head
        // polls the tip over the same transport, so this follows the chain — it is
        // not catch-up-only. It trades the state DB's disk-speed reads for
        // per-block RPC round-trips.
        SourceMode::Rpc {
            jsonrpc_address,
            cookie_path,
            user,
            password,
        } => {
            info!(rpc = %jsonrpc_address, "connecting validator JSON-RPC (Rpc)");
            let rpc = ZebraRpcAdapter::new(rpc_client_from_config(
                jsonrpc_address,
                cookie_path.as_deref(),
                user.as_deref(),
                password.as_deref(),
            )?);
            let validator = ZebraValidator::rpc_only(rpc)
                .with_tip_polling(
                    tip_probe(
                        jsonrpc_address,
                        cookie_path.as_deref(),
                        user.as_deref(),
                        password.as_deref(),
                    )?,
                    TIP_POLL_INTERVAL,
                )
                .await
                .map_err(IndexerError::TipPolling)?;
            select_deployment(client_over(Arc::new(validator)), config).await
        }
    }
}

/// How often the validator is asked for its tip on behalf of the consumers
/// that follow it through a subscription — the finalised indexer, whose
/// steady-state loop extends the index on each observed change. Neither
/// Zebra transport pushes tip changes, so the composite synthesises the
/// subscription by polling; without it the indexer parks at its caught-up
/// height for the life of the process. Blocks arrive about every 75 seconds,
/// so a two-second cadence keeps the finalised boundary within a poll of
/// where it should be at a negligible cost.
const TIP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// A second JSON-RPC handle onto the same validator, for the tip poller: the
/// composite takes it separately so the poller never contends with the
/// composite's own request path.
fn tip_probe(
    jsonrpc_address: &str,
    cookie_path: Option<&Path>,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<ZebraRpcAdapter, IndexerError> {
    Ok(ZebraRpcAdapter::new(rpc_client_from_config(
        jsonrpc_address,
        cookie_path,
        user,
        password,
    )?))
}

/// The one client every consumer shares: the resilient wrapper over one shared
/// validator. Retrying transient failures happens here and nowhere above.
fn client_over<V>(validator: Arc<V>) -> Arc<ValidatorClient<Arc<V>>> {
    Arc::new(ValidatorClient::new(validator, RetryPolicy::default()))
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

/// Select the deployment config names and boot it.
///
/// The one place a runtime value becomes a type: each arm is a fully static
/// shape, and the only thing an arm supplies beyond the deployment is the
/// serving adapter that speaks its use case's protocol. Adding a deployment
/// is adding an arm; the compiler checks the arm's shape at the runtime's
/// `compose`.
async fn select_deployment<C>(
    client: Arc<C>,
    config: DaemonConfig,
) -> Result<JoinHandle<Result<(), IndexerError>>, IndexerError>
where
    // Each arm names what its deployment requires of the validator, as one
    // bundle; the demand its adapter carries then follows from the impls. The
    // bound is the union over the arms — the shared client answers all three.
    // `LightWalletPassthroughSource` is the widest light-wallet bundle (it
    // extends the shared floor the local arm needs with the address ports the
    // passthrough arm relays), so it subsumes the local arm's requirement.
    C: LightWalletPassthroughSource + NodeRpcSource,
{
    let runtime = IndexedDeploymentConfig {
        store: config.store.clone(),
        indexer: config.indexer.clone(),
    };
    let grpc = config.serve.grpc_listen_address;
    let jsonrpc = config.serve.jsonrpc_listen_address;
    let orchestra = match config.deployment {
        DeploymentKind::LightWalletPassthrough => {
            let orchestra =
                boot_indexed::<LightWalletPassthrough, _, C>(client, &runtime, |engine| {
                    GrpcServer::new(LightServe::new(engine), grpc)
                })
                .await?;
            info!(grpc = %grpc, "Zaino runtime booted");
            orchestra
        }
        DeploymentKind::LightWalletLocal => {
            let orchestra = boot_indexed::<LightWalletLocal, _, C>(client, &runtime, |engine| {
                GrpcServer::new(LightServe::new(engine), grpc)
            })
            .await?;
            info!(grpc = %grpc, "Zaino runtime booted");
            orchestra
        }
        DeploymentKind::NodeRpcLocal => {
            // `validateaddress` / `z_validateaddress` are pure functions of an
            // address and a network, so the serving adapter carries the network.
            let network = to_zcash_network(config.network);
            let orchestra = boot_indexed::<NodeRpcLocal, _, C>(client, &runtime, |engine| {
                JsonRpcServer::new(NodeRpc::new(engine, network), jsonrpc)
            })
            .await?;
            info!(jsonrpc = %jsonrpc, "Zaino runtime booted");
            orchestra
        }
    };
    Ok(tokio::spawn(run_until_exit(orchestra)))
}

/// Run until a shutdown signal (clean exit) or a component escalation (fatal
/// → restart). Either way, stop supervising the rest.
async fn run_until_exit(mut orchestra: Orchestra) -> Result<(), IndexerError> {
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

/// Map the daemon's network to `zcash_protocol`'s consensus network, for the
/// address-validation RPCs the node-RPC adapter answers (`validateaddress`,
/// `z_validateaddress`, `z_listunifiedreceivers`).
///
/// `zcash_protocol::consensus::Network` has only `MainNetwork` and
/// `TestNetwork`; it cannot express regtest, so `Regtest` maps to
/// `TestNetwork`. That is exact for transparent addresses, which regtest
/// encodes as testnet does, but not for shielded ones: regtest Sapling and
/// unified addresses carry their own human-readable parts, which a
/// `TestNetwork` validation rejects. On mainnet the mapping is exact.
fn to_zcash_network(network: Network) -> ZcashNetwork {
    match network {
        Network::Mainnet => ZcashNetwork::MainNetwork,
        Network::PubTestnet | Network::Regtest => ZcashNetwork::TestNetwork,
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
