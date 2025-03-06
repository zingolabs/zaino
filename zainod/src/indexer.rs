//! Zingo-Indexer implementation.

use tokio::time::Instant;
use tracing::info;

use zaino_fetch::jsonrpc::connector::test_node_and_return_url;
use zaino_serve::server::{config::GrpcConfig, grpc::TonicServer};
use zaino_state::{
    config::FetchServiceConfig,
    fetch::FetchService,
    indexer::{IndexerService, ZcashService},
    status::StatusType,
};

use crate::{config::IndexerConfig, error::IndexerError};

/// Zingo-Indexer.
pub struct Indexer {
    /// GRPC server.
    server: Option<TonicServer>,
    /// Chain fetch service state process handler..
    service: Option<IndexerService<FetchService>>,
}

impl Indexer {
    /// Starts Indexer service.
    ///
    /// Currently only takes an IndexerConfig.
    pub async fn start(
        config: IndexerConfig,
    ) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
        startup_message();
        info!("Starting Zaino..");
        Indexer::spawn(config).await
    }

    /// Spawns a new Indexer server.
    pub async fn spawn(
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
            " - Connected to node using JsonRPC at address {}.",
            zebrad_uri
        );

        let chain_state_service = IndexerService::<FetchService>::spawn(FetchServiceConfig::new(
            config.validator_listen_address,
            config.validator_cookie_auth,
            config.validator_cookie_path.clone(),
            config.validator_user.clone(),
            config.validator_password.clone(),
            None,
            None,
            config.map_capacity,
            config.map_shard_amount,
            config.db_path.clone(),
            config.db_size,
            config.get_network()?,
            config.no_sync,
            config.no_db,
        ))
        .await?;

        let grpc_server = TonicServer::spawn(
            chain_state_service.inner_ref().get_subscriber(),
            GrpcConfig {
                grpc_listen_address: config.grpc_listen_address,
                tls: config.grpc_tls,
                tls_cert_path: config.tls_cert_path.clone(),
                tls_key_path: config.tls_key_path.clone(),
            },
        )
        .await
        .unwrap();

        let mut indexer = Indexer {
            server: Some(grpc_server),
            service: Some(chain_state_service),
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

        Ok(serve_task)
    }

    /// Checks indexers status and servers internal statuses for either offline of critical error signals.
    fn check_for_critical_errors(&self) -> bool {
        let status = self.status_int();
        status == 5 || status >= 7
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
        if let Some(mut server) = self.server.take() {
            server.close().await;
            server.status.store(StatusType::Offline.into());
        }

        if let Some(service) = self.service.take() {
            let mut service = service.inner();
            service.close();
        }
    }

    /// Returns the indexers current status usize, caliculates from internal statuses.
    fn status_int(&self) -> u16 {
        let service_status = match &self.service {
            Some(service) => service.inner_ref().status(),
            None => return 7,
        };
        let server_status = match &self.server {
            Some(server) => server.status(),
            None => return 7,
        };

        StatusType::combine(service_status, server_status) as u16
    }

    /// Returns the current StatusType of the indexer.
    pub fn status(&self) -> StatusType {
        StatusType::from(self.status_int() as usize)
    }

    /// Logs the indexers status.
    pub fn log_status(&self) {
        let service_status = match &self.service {
            Some(service) => service.inner_ref().status(),
            None => StatusType::Offline,
        };
        let grpc_server_status = match &self.server {
            Some(server) => server.status(),
            None => StatusType::Offline,
        };

        let service_status_symbol = service_status.get_status_symbol();
        let grpc_server_status_symbol = grpc_server_status.get_status_symbol();

        info!(
            "Zaino status check - ChainState Service:{}{} gRPC Server:{}{}",
            service_status_symbol, service_status, grpc_server_status_symbol, grpc_server_status
        );
    }
}

/// Prints Zaino's startup message.
fn startup_message() {
    let welcome_message = r#"
       ░░░░░░░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒░░░▒▒░░░░░
       ░░░░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓░▒▒▒░░
       ░░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒░▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▓▓▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒██▓▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒██▓▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓███▓██▓▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▓▓▓▓▒███▓░▒▓▓████████████████▓▓▒▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▓▓▓▓▒▓████▓▓███████████████████▓▒▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▓▓▓▓▓▒▒▓▓▓▓████████████████████▓▒▓▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▓▓▓▓▓█████████████████████████▓▒▓▓▓▓▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▓▓▓▒▓█████████████████████████▓▓▓▓▓▓▓▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▓▓▓████████████████████████▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▓▒███████████████████████▒▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▓███████████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▓███████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▓██████████▓▓▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒███▓▒▓▓▓▓▓▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▓████▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▒░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
              Thank you for using ZingoLabs Zaino!

       - Donate to us at https://free2z.cash/zingolabs.
       - Submit any security conserns to us at zingodisclosure@proton.me.

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{}", welcome_message);
}
