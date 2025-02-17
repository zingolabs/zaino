//! Zingo-Indexer implementation.

use std::process;
use tokio::time::Instant;
use tracing::info;

use zaino_fetch::jsonrpc::connector::test_node_and_return_url;
use zaino_serve::server::{config::GrpcConfig, grpc::TonicServer};
use zaino_state::{
    config::FetchServiceConfig,
    fetch::FetchService,
    indexer::{IndexerService, ZcashService},
    status::{AtomicStatus, StatusType},
};

use crate::{config::IndexerConfig, error::IndexerError};

/// Holds the status of the server and all its components.
#[derive(Debug, Clone)]
pub struct IndexerStatus {
    /// Status of the indexer.
    pub indexer_status: AtomicStatus,
    /// Status of the chain state service.
    pub service_status: AtomicStatus,
    /// Status of the gRPC server.
    pub grpc_server_status: AtomicStatus,
}

impl IndexerStatus {
    /// Creates a new IndexerStatus.
    pub fn new() -> Self {
        IndexerStatus {
            indexer_status: AtomicStatus::new(StatusType::Offline.into()),
            service_status: AtomicStatus::new(StatusType::Offline.into()),
            grpc_server_status: AtomicStatus::new(StatusType::Offline.into()),
        }
    }

    /// Returns the IndexerStatus.
    pub fn load(&self) -> IndexerStatus {
        self.indexer_status.load();
        self.service_status.load();
        self.grpc_server_status.load();
        self.clone()
    }

    /// Logs the indexers status.
    pub fn log(&self) {
        let statuses = self.load();
        let indexer_status = StatusType::from(statuses.indexer_status);
        let service_status = StatusType::from(statuses.service_status);
        let grpc_server_status = StatusType::from(statuses.grpc_server_status);

        let indexer_status_symbol = indexer_status.get_status_symbol();
        let service_status_symbol = service_status.get_status_symbol();
        let grpc_server_status_symbol = grpc_server_status.get_status_symbol();

        info!(
            "Zaino status check - Indexer:{}{} Service:{}{} gRPC Server:{}{}",
            indexer_status_symbol,
            indexer_status,
            service_status_symbol,
            service_status,
            grpc_server_status_symbol,
            grpc_server_status
        );
    }
}

/// Zingo-Indexer.
pub struct Indexer {
    /// GRPC server.
    server: Option<TonicServer>,
    /// Chain fetch service state process handler..
    service: Option<IndexerService<FetchService>>,
    /// Indexers status.
    status: IndexerStatus,
}

impl Indexer {
    /// Starts Indexer service.
    ///
    /// Currently only takes an IndexerConfig.
    pub async fn start(config: IndexerConfig) -> Result<(), IndexerError> {
        let indexer_status = IndexerStatus::new();
        set_ctrlc(indexer_status.clone());
        startup_message();
        info!("Starting Zaino..");
        if !config.donation_address.is_empty() {
            info!("Instance donation address: {}", config.donation_address);
        }
        Indexer::spawn(config, indexer_status).await?.await??;
        Ok(())
    }

    /// Spawns a new Indexer server.
    pub async fn spawn(
        config: IndexerConfig,
        status: IndexerStatus,
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

        status.indexer_status.store(StatusType::Spawning.into());

        let chain_state_service = IndexerService::<FetchService>::spawn(
            FetchServiceConfig::new(
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
                config.donation_address,
            ),
            status.service_status.clone(),
        )
        .await?;

        let grpc_server = TonicServer::spawn(
            chain_state_service.inner_ref().get_subscriber(),
            GrpcConfig {
                grpc_listen_address: config.grpc_listen_address,
                tls: config.grpc_tls,
                tls_cert_path: config.tls_cert_path.clone(),
                tls_key_path: config.tls_key_path.clone(),
            },
            status.grpc_server_status.clone(),
        )
        .await
        .unwrap();

        let mut indexer = Indexer {
            server: Some(grpc_server),
            service: Some(chain_state_service),
            status,
        };

        indexer
            .status
            .indexer_status
            .store(StatusType::Ready.into());

        // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
        let mut server_interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
        let mut last_log_time = Instant::now();
        let log_interval = tokio::time::Duration::from_secs(10);

        let serve_task = tokio::task::spawn(async move {
            loop {
                indexer.status();

                if last_log_time.elapsed() >= log_interval {
                    indexer.status.log();
                    last_log_time = Instant::now();
                }

                if indexer.check_for_shutdown() {
                    indexer.shutdown().await;
                    return Ok(());
                }
                server_interval.tick().await;
            }
        });

        Ok(serve_task)
    }

    /// Checks indexers online status and servers internal status for closure signal.
    fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        false
    }

    /// Sets the servers to close gracefully.
    async fn shutdown(&mut self) {
        self.status
            .grpc_server_status
            .store(StatusType::Closing.into());
        self.status.service_status.store(StatusType::Closing.into());
        self.status.indexer_status.store(StatusType::Closing.into());

        if let Some(mut server) = self.server.take() {
            server.shutdown().await;
            self.status
                .grpc_server_status
                .store(StatusType::Offline.into());
        }

        if let Some(service) = self.service.take() {
            service.inner().close();
            self.status.service_status.store(StatusType::Offline.into());
        }

        self.status.indexer_status.store(StatusType::Offline.into());
    }

    /// Returns the indexers current status usize.
    pub fn status(&self) -> usize {
        self.status.indexer_status.load()
    }

    /// Returns the indexers current statustype.
    pub fn statustype(&self) -> StatusType {
        StatusType::from(self.status())
    }

    /// Returns the status of the indexer and its parts.
    pub fn statuses(&mut self) -> IndexerStatus {
        self.status.load()
    }
}

fn set_ctrlc(status: IndexerStatus) {
    ctrlc::set_handler(move || {
        status.grpc_server_status.store(StatusType::Closing.into());
        status.service_status.store(StatusType::Closing.into());
        status.indexer_status.store(StatusType::Closing.into());
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");
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
       - Submit any security concerns to us at zingodisclosure@proton.me.

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{}", welcome_message);
}
