//! Zingo-Indexer implementation.

use tokio::time::Instant;
use tracing::info;

use zaino_fetch::jsonrpsee::connector::test_node_and_return_url;
use zaino_serve::server::{
    config::{GrpcConfig, JsonRpcConfig},
    grpc::TonicServer,
    jsonrpc::JsonRpcServer,
};
use zaino_state::{
    BackendConfig, FetchService, IndexerService, LightWalletService, StateService, StatusType,
    ZcashIndexer, ZcashService,
};

use crate::{config::IndexerConfig, error::IndexerError};

/// Zingo-Indexer.
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
    config: IndexerConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    startup_message();
    info!("Starting Zaino..");
    spawn_indexer(config).await
}

/// Spawns a new Indexer server.
pub async fn spawn_indexer(
    config: IndexerConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.check_config()?;
    info!("Checking connection with node..");
    let zebrad_uri = test_node_and_return_url(
        config.validator_listen_address,
        config.validator_cookie_auth,
        config.validator_cookie_path.clone(),
        config.validator_user.clone(),
        config.validator_password.clone(),
    )
    .await?;

    info!(
        " - Connected to node using JsonRPSee at address {}.",
        zebrad_uri
    );
    match BackendConfig::try_from(config.clone()) {
        Ok(BackendConfig::State(state_service_config)) => {
            Indexer::<StateService>::spawn_inner(state_service_config, config).await
        }
        Ok(BackendConfig::Fetch(fetch_service_config)) => {
            Indexer::<FetchService>::spawn_inner(fetch_service_config, config).await
        }
        Err(e) => Err(e),
    }
}

impl<Service: ZcashService + LightWalletService + Send + Sync + 'static> Indexer<Service>
where
    IndexerError: From<<Service::Subscriber as ZcashIndexer>::Error>,
{
    /// Spawns a new Indexer server.
    pub async fn spawn_inner(
        service_config: Service::Config,
        indexer_config: IndexerConfig,
    ) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
        let service = IndexerService::<Service>::spawn(service_config).await?;

        // let read_state_service = IndexerService::<StateService>::spawn(StateServiceConfig::new(
        //     todo!("add zebra config to indexerconfig"),
        //     config.validator_listen_address,
        //     config.validator_cookie_auth,
        //     config.validator_cookie_path,
        //     config.validator_user,
        //     config.validator_password,
        //     None,
        //     None,
        //     config.get_network()?,
        // ))
        // .await?;

        let json_server = match indexer_config.enable_json_server {
            true => Some(
                JsonRpcServer::spawn(
                    service.inner_ref().get_subscriber(),
                    JsonRpcConfig {
                        json_rpc_listen_address: indexer_config.json_rpc_listen_address,
                        enable_cookie_auth: indexer_config.enable_cookie_auth,
                        cookie_dir: indexer_config.cookie_dir,
                    },
                )
                .await
                .unwrap(),
            ),
            false => None,
        };

        let grpc_server = TonicServer::spawn(
            service.inner_ref().get_subscriber(),
            GrpcConfig {
                grpc_listen_address: indexer_config.grpc_listen_address,
                tls: indexer_config.grpc_tls,
                tls_cert_path: indexer_config.tls_cert_path.clone(),
                tls_key_path: indexer_config.tls_key_path.clone(),
            },
        )
        .await
        .unwrap();

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
                    indexer.log_status().await;
                    last_log_time = Instant::now();
                }

                // Check for restart signals.
                if indexer.check_for_critical_errors().await {
                    indexer.close().await;
                    return Err(IndexerError::Restart);
                }

                // Check for shutdown signals.
                if indexer.check_for_shutdown().await {
                    indexer.close().await;
                    return Ok(());
                }

                server_interval.tick().await;
            }
        });

        Ok(serve_task)
    }

    /// Checks indexers status and servers internal statuses for either offline of critical error signals.
    async fn check_for_critical_errors(&self) -> bool {
        let status = self.status_int().await;
        status == 5 || status >= 7
    }

    /// Checks indexers status and servers internal status for closure signal.
    async fn check_for_shutdown(&self) -> bool {
        if self.status_int().await == 4 {
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

    /// Returns the indexers current status usize, caliculates from internal statuses.
    async fn status_int(&self) -> usize {
        let service_status = match &self.service {
            Some(service) => service.inner_ref().status().await,
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
    pub async fn status(&self) -> StatusType {
        StatusType::from(self.status_int().await)
    }

    /// Logs the indexers status.
    pub async fn log_status(&self) {
        let service_status = match &self.service {
            Some(service) => service.inner_ref().status().await,
            None => StatusType::Offline,
        };

        let json_server_status = match &self.server {
            Some(server) => server.status(),
            None => StatusType::Offline,
        };

        let grpc_server_status = match &self.server {
            Some(server) => server.status(),
            None => StatusType::Offline,
        };

        let service_status_symbol = service_status.get_status_symbol();
        let json_server_status_symbol = json_server_status.get_status_symbol();
        let grpc_server_status_symbol = grpc_server_status.get_status_symbol();

        info!(
            "Zaino status check - ChainState Service:{}{} JsonRPC Server:{}{} gRPC Server:{}{}",
            service_status_symbol,
            service_status,
            json_server_status_symbol,
            json_server_status,
            grpc_server_status_symbol,
            grpc_server_status
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
       - For security conserns contact us online on Matrix:
         Admins can be contacted for private messageing at
         #zingolabs:matrix.org which is a public, unencrypted channel.

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{welcome_message}");
}
