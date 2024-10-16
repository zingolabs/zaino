//! Zingo-Indexer implementation.

use std::{
    net::SocketAddr,
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use http::Uri;
use zaino_fetch::jsonrpc::connector::test_node_and_return_uri;
use zaino_serve::server::{
    director::{Server, ServerStatus},
    error::ServerError,
    AtomicStatus, StatusType,
};

use crate::{config::IndexerConfig, error::IndexerError};

/// Holds the status of the server and all its components.
#[derive(Debug, Clone)]
pub struct IndexerStatus {
    indexer_status: AtomicStatus,
    server_status: ServerStatus,
    // block_cache_status: BlockCacheStatus,
}

impl IndexerStatus {
    /// Creates a new IndexerStatus.
    pub fn new(max_workers: u16) -> Self {
        IndexerStatus {
            indexer_status: AtomicStatus::new(5),
            server_status: ServerStatus::new(max_workers),
        }
    }

    /// Returns the IndexerStatus.
    pub fn load(&self) -> IndexerStatus {
        self.indexer_status.load();
        self.server_status.load();
        self.clone()
    }
}

/// Zingo-Indexer.
pub struct Indexer {
    /// Indexer configuration data.
    _config: IndexerConfig,
    /// GRPC server.
    server: Option<Server>,
    // /// Internal block cache.
    // block_cache: BlockCache,
    /// Indexers status.
    status: IndexerStatus,
    /// Online status of the indexer.
    online: Arc<AtomicBool>,
}

impl Indexer {
    /// Starts Indexer service.
    ///
    /// Currently only takes an IndexerConfig.
    pub async fn start(config: IndexerConfig) -> Result<(), IndexerError> {
        let online = Arc::new(AtomicBool::new(true));
        set_ctrlc(online.clone());
        startup_message();
        self::Indexer::start_indexer_service(config, online)
            .await?
            .await?
    }

    /// Launches an Indexer service.
    ///
    /// Spawns an indexer service in a new task.
    pub async fn start_indexer_service(
        config: IndexerConfig,
        online: Arc<AtomicBool>,
    ) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
        // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
        // if config.nym_active {
        //     nym_bin_common::logging::setup_logging();
        // }
        println!("Launching Zingdexer!");
        let mut indexer: Indexer = Indexer::new(config, online.clone()).await?;
        Ok(tokio::task::spawn(async move {
            let server_handle = if let Some(server) = indexer.server.take() {
                Some(server.serve().await)
            } else {
                return Err(IndexerError::MiscIndexerError(
                    "Server Missing! Fatal Error!.".to_string(),
                ));
            };

            indexer.status.indexer_status.store(2);
            loop {
                indexer.status.load();
                // indexer.log_status();
                if indexer.check_for_shutdown() {
                    indexer.status.indexer_status.store(4);
                    indexer.shutdown_components(server_handle).await;
                    indexer.status.indexer_status.store(5);
                    return Ok(());
                }
                interval.tick().await;
            }
        }))
    }

    /// Creates a new Indexer.
    ///
    /// Currently only takes an IndexerConfig.
    async fn new(config: IndexerConfig, online: Arc<AtomicBool>) -> Result<Self, IndexerError> {
        config.check_config()?;
        let status = IndexerStatus::new(config.max_worker_pool_size);
        let tcp_ingestor_listen_addr: Option<SocketAddr> = config
            .listen_port
            .map(|port| SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port));
        let lightwalletd_uri = Uri::builder()
            .scheme("http")
            .authority(format!("localhost:{}", config.lightwalletd_port))
            .path_and_query("/")
            .build()?;
        println!("Checking connection with node..");
        let zebrad_uri = test_node_and_return_uri(
            &config.zebrad_port,
            config.node_user.clone(),
            config.node_password.clone(),
        )
        .await?;
        status.indexer_status.store(0);
        let server = Some(
            Server::spawn(
                config.tcp_active,
                tcp_ingestor_listen_addr,
                config.nym_active,
                config.nym_conf_path.clone(),
                lightwalletd_uri,
                zebrad_uri,
                config.max_queue_size,
                config.max_worker_pool_size,
                config.idle_worker_pool_size,
                status.server_status.clone(),
                online.clone(),
            )
            .await?,
        );
        println!("Server Ready.");
        Ok(Indexer {
            _config: config,
            server,
            status,
            online,
        })
    }

    /// Checks indexers online status and servers internal status for closure signal.
    fn check_for_shutdown(&self) -> bool {
        if self.status() >= 4 {
            return true;
        }
        if !self.check_online() {
            return true;
        }
        false
    }

    /// Sets the servers to close gracefully.
    pub fn shutdown(&mut self) {
        self.status.indexer_status.store(4)
    }

    /// Sets the server's components to close gracefully.
    async fn shutdown_components(
        &mut self,
        server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    ) {
        if let Some(handle) = server_handle {
            self.status.server_status.server_status.store(4);
            handle.await.ok();
        }
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
        self.status.load();
        self.status.clone()
    }

    /// Check the online status on the indexer.
    fn check_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}

fn set_ctrlc(online: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        online.store(false, Ordering::SeqCst);
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");
}

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

****** Please note Zingdexer is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{}", welcome_message);
}
