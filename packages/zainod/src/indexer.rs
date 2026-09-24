//! Zaino : Zingo-Indexer implementation.

use tokio::time::Instant;
use tracing::info;

use zaino_rpc::probe_node;
use zaino_serve::{
    rpc::grpc_routes,
    server::{config::GrpcServerConfig, grpc::TonicServer, jsonrpc::JsonRpcServer},
};
use zaino_state::{
    IndexerService, LightWalletService, NodeBackedIndexerService, ZcashIndexer, ZcashService,
};
use zaino_status::StatusType;

use crate::{config::ZainodConfig, error::IndexerError};

/// Zaino, the Zingo-Indexer.
pub struct Indexer<Service: ZcashService + LightWalletService> {
    /// JsonRPC server.
    ///
    /// Disabled by default.
    json_server: Option<JsonRpcServer>,
    /// GRPC server.
    server: Option<TonicServer>,
    /// The listener settings held back until the index has synced, or `None` once the listeners are bound.
    pending_listeners: Option<PendingListeners>,
    /// Chain fetch service state process handler..
    service: Option<IndexerService<Service>>,
}

/// The gRPC and JSON-RPC listener settings, with any sockets a test harness bound in advance.
struct PendingListeners {
    /// The JSON-RPC server settings, or `None` when the JSON-RPC server is disabled.
    json_server_settings: Option<zaino_serve::server::config::JsonRpcServerConfig>,
    /// The gRPC server settings.
    grpc_config: GrpcServerConfig,
    /// A gRPC socket bound in advance by a test harness.
    grpc_listener: Option<std::net::TcpListener>,
    /// A JSON-RPC socket bound in advance by a test harness.
    json_listener: Option<std::net::TcpListener>,
}

/// Starts Indexer service.
///
/// Currently only takes an IndexerConfig.
pub async fn start_indexer(
    config: ZainodConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    startup_message();
    info!("Starting Zaino");
    spawn_indexer(config).await
}

/// Spawns a new Indexer server.
pub async fn spawn_indexer(
    config: ZainodConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.check_config()?;
    info!(
        address = %config.validator_settings.validator_jsonrpc_listen_address,
        "Checking connection with node"
    );
    if let Some(donation_address) = &config.donation_address {
        info!(%donation_address, "instance donation address");
    }
    let zebrad_uri = probe_node(
        &config.validator_settings.validator_jsonrpc_listen_address,
        config.validator_settings.validator_cookie_path.as_deref(),
        config.validator_settings.validator_user.clone(),
        config.validator_settings.validator_password.clone(),
    )
    .await?;

    info!(uri = %zebrad_uri, "Connected to node via JsonRPSee");

    let service_config = crate::config::build_common(config.clone());
    Indexer::<NodeBackedIndexerService>::launch_inner(service_config, config)
        .await
        .map(|res| res.0)
}

impl<Service: ZcashService + LightWalletService + Send + Sync + 'static> Indexer<Service>
where
    IndexerError: From<<Service::Subscriber as ZcashIndexer>::Error>,
{
    /// Spawns a new Indexer server.
    // TODO: revise whether returning the subscriber here is the best way to access the service after the indexer is spawned.
    pub async fn launch_inner(
        service_config: Service::Config,
        indexer_config: ZainodConfig,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), IndexerError>>,
            Service::Subscriber,
        ),
        IndexerError,
    > {
        Self::launch_inner_impl(service_config, indexer_config, None, None).await
    }

    /// Launches the indexer on pre-bound listeners (test-only).
    ///
    /// The harness binds `127.0.0.1:0` for the gRPC server (and the JSON-RPC
    /// server when enabled), reads the OS-assigned ports, and hands the open
    /// sockets here — eliminating the pick-a-port / bind-later race that
    /// otherwise flakes under parallel test execution. `json_listener` must be
    /// `Some` exactly when `indexer_config.json_server_settings` is `Some`.
    #[cfg(feature = "test_dependencies")]
    pub async fn launch_inner_with_listeners(
        service_config: Service::Config,
        indexer_config: ZainodConfig,
        grpc_listener: std::net::TcpListener,
        json_listener: Option<std::net::TcpListener>,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), IndexerError>>,
            Service::Subscriber,
        ),
        IndexerError,
    > {
        Self::launch_inner_impl(
            service_config,
            indexer_config,
            Some(grpc_listener),
            json_listener,
        )
        .await
    }

    async fn launch_inner_impl(
        service_config: Service::Config,
        indexer_config: ZainodConfig,
        grpc_listener: Option<std::net::TcpListener>,
        json_listener: Option<std::net::TcpListener>,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), IndexerError>>,
            Service::Subscriber,
        ),
        IndexerError,
    > {
        let service = IndexerService::<Service>::spawn(service_config).await?;
        let service_subscriber = service.inner_ref().get_subscriber();

        let pending_listeners = PendingListeners {
            json_server_settings: indexer_config.json_server_settings,
            grpc_config: GrpcServerConfig {
                listen_address: indexer_config.grpc_settings.listen_address,
                tls: indexer_config.grpc_settings.tls,
            },
            grpc_listener,
            json_listener,
        };

        let mut indexer = Self {
            json_server: None,
            server: None,
            pending_listeners: Some(pending_listeners),
            service: Some(service),
        };
        info!("serving nothing until the finalised state reaches the finalised floor");

        let mut server_interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
        let mut last_log_time = Instant::now();
        let log_interval = tokio::time::Duration::from_secs(10);

        let serve_task = tokio::task::spawn(async move {
            let shutdown = shutdown_signal();
            tokio::pin!(shutdown);
            loop {
                // Every tick (100ms): the heartbeat `/livez` answers from
                #[cfg(feature = "prometheus")]
                crate::admin::heartbeat();

                if let Err(error) = indexer.bind_listeners_once_synced().await {
                    indexer.close().await;
                    return Err(error);
                }

                // Log the servers status.
                if last_log_time.elapsed() >= log_interval {
                    indexer.log_status();
                    last_log_time = Instant::now();
                }

                // Check for restart signals.
                if indexer.check_for_critical_errors() {
                    indexer.close().await;
                    return Err(IndexerError::Restart);
                }

                // Check for shutdown signals.
                if indexer.check_for_shutdown() {
                    indexer.close().await;
                    return Ok(());
                }

                tokio::select! {
                    _ = server_interval.tick() => {}
                    // Pod teardown = SIGTERM; same graceful close, so the db and
                    // mempool are not killed mid-write
                    _ = &mut shutdown => {
                        info!("received shutdown signal; closing Zaino gracefully");
                        indexer.close().await;
                        return Ok(());
                    }
                }
            }
        });

        Ok((serve_task, service_subscriber.inner()))
    }

    /// Binds the gRPC and JSON-RPC listeners once the index has synced, and does nothing before that or after they are bound.
    async fn bind_listeners_once_synced(&mut self) -> Result<(), IndexerError> {
        let Some(service) = &self.service else {
            return Ok(());
        };
        if !service.inner_ref().is_synced() {
            return Ok(());
        }
        let Some(pending) = self.pending_listeners.take() else {
            return Ok(());
        };

        self.json_server = match pending.json_server_settings {
            Some(json_server_config) => Some(match pending.json_listener {
                #[cfg(feature = "test_dependencies")]
                Some(listener) => {
                    JsonRpcServer::spawn_from_listener(
                        service.inner_ref().get_subscriber(),
                        json_server_config,
                        listener,
                    )
                    .await?
                }
                _ => {
                    JsonRpcServer::spawn(service.inner_ref().get_subscriber(), json_server_config)
                        .await?
                }
            }),
            None => None,
        };

        self.server = Some(match pending.grpc_listener {
            #[cfg(feature = "test_dependencies")]
            Some(listener) => {
                TonicServer::spawn_from_listener_with_routes(
                    |shutdown| grpc_routes(service.inner_ref().get_subscriber(), shutdown),
                    pending.grpc_config,
                    listener,
                )
                .await?
            }
            _ => {
                TonicServer::spawn_with_routes(
                    |shutdown| grpc_routes(service.inner_ref().get_subscriber(), shutdown),
                    pending.grpc_config,
                )
                .await?
            }
        });

        #[cfg(feature = "prometheus")]
        crate::admin::mark_ready();
        info!("index synced; gRPC and JSON-RPC listeners bound");
        Ok(())
    }

    /// Checks indexers status and servers internal statuses for either offline of critical error signals.
    fn check_for_critical_errors(&self) -> bool {
        let status = self.status_int();
        if status == 5 || status >= 7 {
            let service_status = self
                .service
                .as_ref()
                .map(|s| s.inner_ref().status())
                .unwrap_or(StatusType::Offline);
            let server_status = self
                .server
                .as_ref()
                .map(|s| s.status())
                .unwrap_or(StatusType::Offline);
            tracing::error!(
                combined_status = status,
                ?service_status,
                ?server_status,
                "check_for_critical_errors triggered"
            );
            return true;
        }
        false
    }

    /// Checks indexers status and servers internal status for closure signal.
    fn check_for_shutdown(&self) -> bool {
        if self.status_int() == 4 {
            return true;
        }
        false
    }

    /// Sets the servers to close gracefully.
    async fn close(&mut self) {
        if let Some(mut json_server) = self.json_server.take() {
            json_server.close().await;
            json_server.status.store(StatusType::Offline);
        }

        if let Some(mut server) = self.server.take() {
            server.close().await;
            server.status.store(StatusType::Offline);
        }

        if let Some(service) = self.service.take() {
            let mut service = service.inner();
            service.close();
        }
    }

    /// Returns the indexers current status usize, calculates from internal statuses.
    fn status_int(&self) -> usize {
        let service_status = match &self.service {
            Some(service) => service.inner_ref().status(),
            None => return 7,
        };

        let json_server_status = self
            .json_server
            .as_ref()
            .map(|json_server| json_server.status());

        let mut server_status = match (&self.server, &self.pending_listeners) {
            (Some(server), _) => server.status(),
            (None, Some(_)) => StatusType::Syncing,
            (None, None) => return 7,
        };

        if let Some(json_status) = json_server_status {
            server_status = StatusType::combine(server_status, json_status);
        }

        usize::from(StatusType::combine(service_status, server_status))
    }

    /// Returns the current StatusType of the indexer.
    pub fn status(&self) -> StatusType {
        StatusType::from(self.status_int())
    }

    /// Logs the indexers status.
    pub fn log_status(&self) {
        let service_status = match &self.service {
            Some(service) => service.inner_ref().status(),
            None => StatusType::Offline,
        };

        let json_server_status = match &self.json_server {
            Some(json_server) => json_server.status(),
            None => StatusType::Offline,
        };

        let grpc_server_status = match &self.server {
            Some(server) => server.status(),
            None => StatusType::Offline,
        };

        info!(
            chain_state = %service_status,
            json_rpc = %json_server_status,
            grpc = %grpc_server_status,
            "Zaino status check"
        );
    }
}

/// Resolves on SIGTERM (pod teardown) or ctrl-c; ctrl-c only off unix
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(%e, "could not install SIGTERM handler; falling back to ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Prints Zaino's startup message.
fn startup_message() {
    let welcome_message = r#"
       ░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓░▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒░▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒██▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒██▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓███▓██▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▓▓▓▓▒███▓░▒▓▓████████████████▓▓▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▓▓▓▓▒▓████▓▓███████████████████▓▒▓▓▒▒▒▒
       ▒▒▒▒▒▓▓▓▓▓▒▒▓▓▓▓████████████████████▓▒▓▓▓▒▒▒▒
       ▒▒▒▒▒▓▓▓▓▓█████████████████████████▓▒▓▓▓▓▓▒▒▒
       ▒▒▒▒▓▓▓▒▓█████████████████████████▓▓▓▓▓▓▓▓▒▒▒
       ▒▒▒▒▒▓▓▓████████████████████████▓▓▓▓▓▓▓▓▓▒▒▒▒
       ▒▒▒▒▒▓▒███████████████████████▒▓▓▓▓▓▓▓▓▓▓▒▒▒▒
       ▒▒▒▒▒▒▓███████████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒
       ▒▒▒▒▒▒▓███████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▓██████████▓▓▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒
       ▒▒▒▒███▓▒▓▓▓▓▓▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▓████▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
             Thank you for using ZingoLabs Zaino!

       - Donate to us at https://free2z.cash/zingolabs.

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{welcome_message}");
}
