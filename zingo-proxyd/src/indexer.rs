//! Zingo-Indexer implementation.

use std::{
    net::{Ipv4Addr, SocketAddr},
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use http::Uri;
use zingo_rpc::{
    jsonrpc::connector::test_node_and_return_uri,
    server::{AtomicStatus, Server, ServerStatus},
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
    /// Indexer onfuguration data.
    config: IndexerConfig,
    /// GRPC server.
    server: Server,
    // Internal block cache.
    // block_cache: BlockCache,
    /// Indexers status.
    status: IndexerStatus,
    /// Online status of the indexer.
    online: Arc<AtomicBool>,
}

impl Indexer {
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
            Some("xxxxxx".to_string()),
            Some("xxxxxx".to_string()),
        )
        .await?;
        status.indexer_status.store(0);
        let server = Server::spawn(
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
        .await?;
        Ok(Indexer {
            config,
            server,
            status,
            online,
        })
    }

    /// Start an Indexer service.
    ///
    /// Currently only takes an IndexerConfig.
    pub async fn start(config: IndexerConfig) -> Result<(), IndexerError> {
        // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));

        let online = Arc::new(AtomicBool::new(true));
        set_ctrlc(online.clone());
        nym_bin_common::logging::setup_logging();

        startup_message();

        println!("Launching Zingdexer!");
        let indexer: Indexer = Indexer::new(config, online.clone()).await?;
        let server_handle = indexer.server.serve().await;

        indexer.status.indexer_status.store(2);
        while online.load(Ordering::SeqCst) {
            indexer.status.load();
            //printout statuses
            //check for shutdown
            interval.tick().await;
        }
        Ok(())
    }

    // /// Closes the Indexer Gracefully.
    // pub async fn shutdown(&self) (

    // )
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
            Thank you for using ZingoLabs Zingdexer!     

       - Donate to us at https://free2z.cash/zingolabs.
       - Submit any security conserns to us at zingodisclosure@proton.me.

****** Please note Zingdexer is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{}", welcome_message);
}
