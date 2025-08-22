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
    config::{TestConfigBuilder},
    ports::TestPorts,
    validator::LocalNet,
};

/// Test environment orchestrator.
pub struct TestManager {
    /// Test environment specification.
    pub env: TestEnvironment,
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
    /// Launch test environment from topology specification.
    pub async fn launch(env: TestEnvironment) -> Result<Self, std::io::Error> {
        // Validation
        if (env.validator.kind == ValidatorKind::Zcashd)
            && env
                .indexer
                .as_ref()
                .map_or(false, |i| i.backend_mode == BackendMode::State)
        {
            return Err(std::io::Error::other(
                "Cannot use state backend with zcashd.",
            ));
        }

        if env
            .clients
            .as_ref()
            .map_or(false, |c| c.enable_lightclients)
            && env.indexer.is_none()
        {
            return Err(std::io::Error::other(
                "Cannot enable clients when indexer is not enabled.",
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
        let local_net = LocalNet::launch_from_env(&env, &ports).await?;

        // 3. Launch indexer if requested
        let (zaino_handle, json_server_cookie_dir) = if env.indexer.is_some() {
            let (handle, cookie_dir) = Self::launch_indexer(&env, &ports).await?;
            (Some(handle), cookie_dir)
        } else {
            (None, None)
        };

        // 4. Launch clients if requested
        let clients = if env
            .clients
            .as_ref()
            .map_or(false, |c| c.enable_lightclients)
        {
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
            env,
            ports,
            local_net,
            zaino_handle,
            json_server_cookie_dir,
            clients,
        })
    }

    /// Launch indexer.
    async fn launch_indexer(
        env: &TestEnvironment,
        ports: &TestPorts,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>,
            Option<PathBuf>,
        ),
        std::io::Error,
    > {
        let indexer_spec = env.indexer.as_ref().unwrap();

        // Allocate additional ports if needed
        let mut ports = ports.clone();
        ports.with_zaino_ports()?;

        let zaino_grpc_address = ports.zaino_grpc.unwrap();
        let zaino_json_address = ports.zaino_json.unwrap();

        let json_server_cookie_dir = if indexer_spec.enable_json_server {
            Some(default_ephemeral_cookie_path())
        } else {
            None
        };

        let mut indexer_config = Self::build_indexer_config(
            env,
            &ports,
            zaino_grpc_address,
            zaino_json_address,
            json_server_cookie_dir.clone(),
        );

        // Apply any customizations
        for customizer in &env.indexer_customizers {
            customizer(&mut indexer_config);
        }

        let handle = zainodlib::indexer::spawn_indexer(indexer_config)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to spawn indexer: {}", e)))?;

        // Give the server time to launch
        tokio::time::sleep(Duration::from_secs(3)).await;

        Ok((handle, json_server_cookie_dir))
    }

    /// Build production IndexerConfig from test specification.
    fn build_indexer_config(
        env: &TestEnvironment,
        ports: &TestPorts,
        zaino_grpc_address: SocketAddr,
        zaino_json_address: SocketAddr,
        _json_server_cookie_dir: Option<PathBuf>,
    ) -> IndexerConfig {
        let indexer_spec = env.indexer.as_ref().unwrap();

        let backend = Self::build_backend_config(env, ports);

        let server = ServerConfig {
            json_rpc: if indexer_spec.enable_json_server {
                Some(JsonRpcConfig {
                    listen_address: zaino_json_address,
                    auth: env.auth.server_auth.clone(),
                })
            } else {
                None
            },
            grpc: GrpcConfig {
                listen_address: zaino_grpc_address,
                tls: TlsConfig::Disabled,
            },
        };

        IndexerConfig {
            network: env.validator.network,
            backend,
            server,
            service: ServiceConfig::default(),
            storage: StorageConfig {
                cache: CacheConfig {
                    capacity: None,
                    shard_amount: None,
                },
                database: DatabaseConfig {
                    path: ports.zaino_db.clone(),
                    size: None,
                },
            },
            debug: indexer_spec.testing_flags.clone().into(),
        }
    }

    /// Build production BackendConfig from test specification.
    fn build_backend_config(env: &TestEnvironment, ports: &TestPorts) -> BackendConfig {
        let indexer_spec = env.indexer.as_ref().unwrap();

        match (env.validator.kind, indexer_spec.backend_mode) {
            (ValidatorKind::Zebrd, BackendMode::State) => BackendConfig::LocalZebra {
                rpc_address: ports.validator_rpc,
                auth: env.auth.validator_auth.clone(),
                zebra_state: ZebradStateConfig::default(),
                indexer_rpc_address: ports.validator_grpc,
                zebra_database: DatabaseConfig {
                    path: ports.zebra_db.clone(),
                    size: None,
                },
            },
            (ValidatorKind::Zebrd, BackendMode::Fetch) => BackendConfig::RemoteZebra {
                rpc_address: ports.validator_rpc,
                auth: env.auth.validator_auth.clone(),
            },
            (ValidatorKind::Zcashd, BackendMode::Fetch) => BackendConfig::RemoteZcashd {
                rpc_address: ports.validator_rpc,
                auth: env.auth.validator_auth.clone(),
            },
            (ValidatorKind::Zcashd, BackendMode::State) => {
                panic!("State backend not supported with zcashd")
            }
        }
    }

    /// Get BackendConfig representing this test environment.
    pub fn backend_config(&self) -> BackendConfig {
        Self::build_backend_config(&self.env, &self.ports)
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
            let request = match &self.env.auth.validator_auth {
                JsonRpcAuth::Disabled => request,
                JsonRpcAuth::Cookie(cookie_auth) => {
                    if !cookie_auth.path.as_os_str().is_empty() {
                        // For cookie auth, we'd need to read the cookie file
                        // For now, just proceed without auth for simplicity
                        request
                    } else {
                        request
                    }
                }
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
