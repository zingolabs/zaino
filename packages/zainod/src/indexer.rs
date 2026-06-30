//! Zaino : Zingo-Indexer implementation.

use tokio::time::Instant;
use tracing::info;

use zaino_fetch::jsonrpsee::connector::test_node_and_return_url;
use zaino_serve::{
    rpc::grpc_routes,
    server::{config::GrpcServerConfig, grpc::TonicServer, jsonrpc::JsonRpcServer},
};
#[allow(deprecated)]
use zaino_state::{
    BackendType, FetchService, FetchServiceConfig, IndexerService, LightWalletService,
    StateService, StateServiceConfig, StatusType, ZcashIndexer, ZcashService,
};

use crate::{config::ZainodConfig, error::IndexerError};

/// Zaino, the Zingo-Indexer.
pub struct Indexer<Service: ZcashService + LightWalletService> {
    /// JsonRPC server.
    ///
    /// Disabled by default.
    json_server: Option<JsonRpcServer>,
    /// GRPC server.
    server: Option<TonicServer>,
    /// Chain fetch service state process handler..
    service: Option<IndexerService<Service>>,
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
    let zebrad_uri = test_node_and_return_url(
        &config.validator_settings.validator_jsonrpc_listen_address,
        config.validator_settings.validator_cookie_path.clone(),
        config.validator_settings.validator_user.clone(),
        config.validator_settings.validator_password.clone(),
    )
    .await?;

    info!(uri = %zebrad_uri, "Connected to node via JsonRPSee");

    #[allow(deprecated)]
    match config.backend {
        BackendType::State => {
            let state_config = StateServiceConfig::try_from(config.clone())?;
            Indexer::<StateService>::launch_inner(state_config, config)
                .await
                .map(|res| res.0)
        }
        BackendType::Fetch => {
            let fetch_config = FetchServiceConfig::try_from(config.clone())?;
            Indexer::<FetchService>::launch_inner(fetch_config, config)
                .await
                .map(|res| res.0)
        }
    }
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

        let json_server = match indexer_config.json_server_settings {
            Some(json_server_config) => Some(match json_listener {
                #[cfg(feature = "test_dependencies")]
                Some(listener) => JsonRpcServer::spawn_from_listener(
                    service.inner_ref().get_subscriber(),
                    json_server_config,
                    listener,
                )
                .await
                .unwrap(),
                _ => JsonRpcServer::spawn(service.inner_ref().get_subscriber(), json_server_config)
                    .await
                    .unwrap(),
            }),
            None => None,
        };

        let routes = grpc_routes(service.inner_ref().get_subscriber());
        let grpc_config = GrpcServerConfig {
            listen_address: indexer_config.grpc_settings.listen_address,
            tls: indexer_config.grpc_settings.tls,
        };
        let grpc_server = match grpc_listener {
            #[cfg(feature = "test_dependencies")]
            Some(listener) => TonicServer::spawn_from_listener(routes, grpc_config, listener)
                .await
                .unwrap(),
            _ => TonicServer::spawn(routes, grpc_config).await.unwrap(),
        };

        let mut indexer = Self {
            json_server,
            server: Some(grpc_server),
            service: Some(service),
        };

        let mut server_interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
        let mut last_log_time = Instant::now();
        let log_interval = tokio::time::Duration::from_secs(10);

        let serve_task = tokio::task::spawn(async move {
            loop {
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

                server_interval.tick().await;
            }
        });

        Ok((serve_task, service_subscriber.inner()))
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

        let mut server_status = match &self.server {
            Some(server) => server.status(),
            None => return 7,
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
