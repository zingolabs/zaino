//! JSON server test manager for validator + indexer + JSON server tests.
//!
//! This manager provides a complete JSON-RPC server testing environment with
//! validator, indexer, JSON server, and optional lightclients.

use crate::{
    clients::Clients,
    config::{JsonRpcAuthConfig, JsonServerTestConfig, TestConfig},
    manager::traits::{
        ConfigurableBuilder, LaunchManager, WithClients, WithIndexer, WithValidator,
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use zaino_commons::config::Network;
use zainodlib::config::IndexerConfig;
use zainodlib::error::IndexerError;

/// Test manager for JSON server tests (validator + indexer + JSON server).
///
/// This manager provides JSON-RPC server testing capabilities with optional
/// lightclient support based on configuration.
#[derive(Debug)]
pub struct JsonServerTestManager {
    pub local_net: LocalNet,
    pub ports: TestPorts,
    pub network: Network,
    pub indexer_config: IndexerConfig,
    pub indexer_handle: JoinHandle<Result<(), IndexerError>>,
    pub json_server_cookie_dir: Option<PathBuf>,
    pub clients: Option<Clients>, // Optional for JSON server tests
}

impl WithValidator for JsonServerTestManager {
    fn local_net(&self) -> &LocalNet {
        &self.local_net
    }

    fn local_net_mut(&mut self) -> &mut LocalNet {
        &mut self.local_net
    }

    fn validator_rpc_address(&self) -> SocketAddr {
        self.ports.validator_rpc
    }

    fn validator_grpc_address(&self) -> SocketAddr {
        self.ports.validator_grpc
    }

    fn network(&self) -> &Network {
        &self.network
    }

    async fn close(&mut self) {
        // Close indexer first
        self.indexer_handle.abort();
        // Then close validator using default implementation
        use crate::validator::Validator as _;
        self.local_net_mut().stop();
    }
}

// Conditional implementation of WithClients only when clients are present
impl WithClients for JsonServerTestManager {
    fn clients(&self) -> &Clients {
        self.clients
            .as_ref()
            .expect("JsonServerTestManager was not configured with clients")
    }

    fn clients_mut(&mut self) -> &mut Clients {
        self.clients
            .as_mut()
            .expect("JsonServerTestManager was not configured with clients")
    }
}

impl WithIndexer for JsonServerTestManager {
    fn indexer_config(&self) -> &IndexerConfig {
        &self.indexer_config
    }

    fn zaino_grpc_address(&self) -> Option<SocketAddr> {
        self.ports.zaino_grpc
    }

    fn zaino_json_address(&self) -> Option<SocketAddr> {
        self.ports.zaino_json
    }

    fn json_server_cookie_dir(&self) -> Option<&PathBuf> {
        self.json_server_cookie_dir.as_ref()
    }

    fn indexer_handle(&self) -> &JoinHandle<Result<(), IndexerError>> {
        &self.indexer_handle
    }
}

impl JsonServerTestManager {
    /// Check if this manager has lightclients available.
    pub fn has_clients(&self) -> bool {
        self.clients.is_some()
    }
}

/// Builder for JsonServerTestManager.
#[derive(Debug, Clone)]
pub struct JsonServerTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
    enable_cookie_auth: bool,
    enable_clients: bool,
}

impl Default for JsonServerTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
            enable_cookie_auth: true, // Common for JSON server tests
            enable_clients: false,    // Optional for JSON server tests
        }
    }
}

impl JsonServerTestsBuilder {
    /// Enable cookie-based authentication.
    pub fn with_cookie_auth(mut self) -> Self {
        self.enable_cookie_auth = true;
        self
    }

    /// Disable authentication.
    pub fn no_auth(mut self) -> Self {
        self.enable_cookie_auth = false;
        self
    }

    /// Enable lightclients for wallet operations.
    pub fn with_clients(mut self) -> Self {
        self.enable_clients = true;
        self
    }
}

impl ConfigurableBuilder for JsonServerTestsBuilder {
    type Manager = JsonServerTestManager;
    type Config = JsonServerTestConfig;

    fn build_config(&self) -> Self::Config {
        let json_auth = if self.enable_cookie_auth {
            // Generate a temporary directory for cookie authentication
            let temp_dir =
                std::env::temp_dir().join(format!("zaino_cookie_{}", std::process::id()));
            JsonRpcAuthConfig::Cookie(temp_dir)
        } else {
            JsonRpcAuthConfig::None
        };

        JsonServerTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
            indexer: IndexerConfig::default(),
            json_auth,
            enable_clients: self.enable_clients,
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        let config = self.build_config();
        config.launch_manager().await
    }

    fn validator(mut self, kind: ValidatorKind) -> Self {
        self.validator_kind = kind;
        self
    }

    fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    fn chain_cache(mut self, path: PathBuf) -> Self {
        self.chain_cache = Some(path);
        self
    }
}
