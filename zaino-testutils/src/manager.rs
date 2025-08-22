//! Test environment orchestration and management.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
use tracing_subscriber::EnvFilter;
use zaino_commons::config::{
    BackendConfig, CacheConfig, DatabaseConfig, GrpcConfig, JsonRpcAuth, JsonRpcConfig,
    ServiceConfig, StorageConfig, TlsConfig, ZebradStateConfig,
};
use zainodlib::config::{default_ephemeral_cookie_path, IndexerConfig, ServerConfig};
use zingo_infra_services::validator::Validator as _;

use crate::{
    clients::Clients,
    config::TestConfigBuilder,
    ports::TestPorts,
    validator::LocalNet,
};

/// Test environment orchestrator.
pub struct TestManager {
    /// Indexer configuration.
    pub config: IndexerConfig,
    /// Enable indexer flag.
    pub enable_indexer: bool,
    /// Enable lightclients flag.
    pub enable_lightclients: bool,
    /// Validator chain cache directory.
    pub chain_cache: Option<PathBuf>,
    /// Network ports and paths.
    pub ports: TestPorts,
    /// Validator network.
    pub local_net: LocalNet,
    /// Zaino indexer handle.
    pub zaino_handle: Option<tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>>,
    /// JSON server cookie directory.
    pub json_server_cookie_dir: Option<PathBuf>,
    /// Zingolib lightclients.
    pub clients: Option<Clients>,
}

impl TestManager {
    /// Launch test environment from configuration builder.
    pub async fn launch(builder: TestConfigBuilder) -> Result<Self, std::io::Error> {
        // Extract parts from builder
        let (mut config, enable_indexer, enable_lightclients, chain_cache) = builder.into_parts();
        
        // Validation: Can't enable clients without indexer (gRPC server needed)
        if enable_lightclients && !enable_indexer {
            return Err(std::io::Error::other(
                "Cannot enable lightclients without indexer (gRPC server needed).",
            ));
        }

        // Initialize logging
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
            .with_target(true)
            .try_init();

        // 1. Allocate ports
        let ports = TestPorts::allocate().await?;

        // 2. Launch validator
        let local_net = LocalNet::launch_from_config(&config, &chain_cache, &ports).await?;

        // 3. Launch indexer if requested
        let (zaino_handle, json_server_cookie_dir) = if enable_indexer {
            // Update config with real ports
            Self::update_config_with_ports(&mut config, &ports);
            let (handle, cookie_dir) = Self::launch_indexer(&config, &ports).await?;
            (Some(handle), cookie_dir)
        } else {
            (None, None)
        };

        // 4. Launch clients if requested
        let clients = if enable_lightclients {
            let zaino_grpc_port = ports
                .zaino_grpc
                .ok_or_else(|| {
                    std::io::Error::other("Zaino gRPC address not available for clients")
                })?
                .port();
            Some(Clients::launch(zaino_grpc_port).await?)
        } else {
            None
        };

        Ok(Self {
            config,
            enable_indexer,
            enable_lightclients,
            chain_cache,
            ports,
            local_net,
            zaino_handle,
            json_server_cookie_dir,
            clients,
        })
    }

    /// Update IndexerConfig placeholder ports with real allocated ports.
    fn update_config_with_ports(config: &mut IndexerConfig, ports: &TestPorts) {
        // Update backend RPC addresses with real validator port
        match &mut config.backend {
            BackendConfig::LocalZebra { rpc_address, indexer_rpc_address, .. } => {
                *rpc_address = ports.validator_rpc;
                if let Some(zaino_grpc) = ports.zaino_grpc {
                    *indexer_rpc_address = zaino_grpc;
                }
            },
            BackendConfig::RemoteZebra { rpc_address, .. } 
            | BackendConfig::RemoteZcashd { rpc_address, .. } 
            | BackendConfig::RemoteZainod { rpc_address, .. } => {
                *rpc_address = ports.validator_rpc;
            },
        }

        // Update server addresses with real zaino ports
        if let Some(zaino_grpc) = ports.zaino_grpc {
            config.server.grpc.listen_address = zaino_grpc;
        }
        
        if let Some(ref mut json_rpc) = config.server.json_rpc {
            if let Some(zaino_json) = ports.zaino_json {
                json_rpc.listen_address = zaino_json;
            }
        }
    }

    /// Launch indexer.
    async fn launch_indexer(
        config: &IndexerConfig,
        ports: &TestPorts,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>,
            Option<PathBuf>,
        ),
        std::io::Error,
    > {
        // Allocate additional ports if needed
        let mut ports = ports.clone();
        ports.with_zaino_ports()?;

        // Determine JSON server cookie directory
        let json_server_cookie_dir = if config.server.json_rpc.is_some() {
            Some(default_ephemeral_cookie_path())
        } else {
            None
        };

        // Config is already properly configured with real ports
        let indexer_config = config.clone();

        let handle = zainodlib::indexer::spawn_indexer(indexer_config)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to spawn indexer: {}", e)))?;

        // Give the server time to launch
        tokio::time::sleep(Duration::from_secs(3)).await;

        Ok((handle, json_server_cookie_dir))
    }


    /// Get BackendConfig representing this test environment.
    pub fn backend_config(&self) -> &BackendConfig {
        &self.config.backend
    }

    /// Generates `blocks` regtest blocks.
    /// Adds a delay between blocks to allow zaino / zebra to catch up with test.
    pub async fn generate_blocks_with_delay(&self, blocks: u32) {
        for _ in 0..blocks {
            self.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Waits for the validator to be ready by polling its JSON-RPC interface.
    /// Returns Ok(()) when the validator responds to a `getinfo` request.
    pub async fn wait_for_validator_ready(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use std::time::{Duration, Instant};

        let timeout = Duration::from_secs(30);
        let start = Instant::now();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;

        let validator_url = format!("http://{}", self.ports.validator_rpc);

        while start.elapsed() < timeout {
            let request_body = r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#;

            let request = client
                .post(&validator_url)
                .header("Content-Type", "application/json")
                .body(request_body);

            // Add authentication if configured
            let validator_auth = match &self.config.backend {
                BackendConfig::LocalZebra { auth, .. } 
                | BackendConfig::RemoteZebra { auth, .. } => auth.get_auth_header(),
                BackendConfig::RemoteZcashd { auth, .. } 
                | BackendConfig::RemoteZainod { auth, .. } => auth.get_auth_header(),
            };
            
            let request = match validator_auth {
                Ok(Some(auth_header)) => request.header(auth_header.key(), auth_header.value()),
                Ok(None) => request, // No auth
                Err(_) => request, // Auth error - proceed without auth for now
            };

            if let Ok(response) = request.send().await {
                if response.status().is_success() {
                    println!("Validator is ready after {:?}", start.elapsed());
                    return Ok(());
                }
            }

            // Wait 100ms before next attempt
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Err(format!("Validator not ready after {:?}", timeout).into())
    }

    /// Closes the TestManager.
    pub async fn close(&mut self) {
        if let Some(handle) = self.zaino_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for TestManager {
    fn drop(&mut self) {
        if let Some(handle) = &self.zaino_handle {
            handle.abort();
        };
    }
}
