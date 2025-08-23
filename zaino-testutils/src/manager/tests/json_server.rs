//! JSON server test manager for validator + indexer + JSON server tests.
//!
//! This manager provides a complete JSON-RPC server testing environment with
//! validator, indexer, JSON server, and optional lightclients.

use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use zaino_commons::config::{IndexerConfig, Network};
use zainodlib::error::IndexerError;
use crate::{
    validator::{LocalNet, ValidatorKind},
    ports::TestPorts,
    clients::Clients,
    config::{JsonServerTestConfig, JsonRpcAuthConfig, TestConfig},
    manager::{
        traits::{WithValidator, WithClients, WithIndexer, ConfigurableBuilder, TestConfiguration},
        factories::{FetchServiceBuilder, StateServiceBuilder, BlockCacheBuilder},
    },
};

/// Test manager for JSON server tests (validator + indexer + JSON server).
///
/// This manager provides JSON-RPC server testing capabilities with optional
/// lightclient support based on configuration.
#[derive(Debug)]
pub struct JsonServerTestManager {
    local_net: LocalNet,
    ports: TestPorts,
    network: Network,
    indexer_config: IndexerConfig,
    indexer_handle: JoinHandle<Result<(), IndexerError>>,
    json_server_cookie_dir: Option<PathBuf>,
    clients: Option<Clients>, // Optional for JSON server tests
}

impl WithValidator for JsonServerTestManager {
    fn validator_rpc_address(&self) -> SocketAddr {
        todo!("Return validator RPC address from ports")
    }
    
    fn validator_grpc_address(&self) -> SocketAddr {
        todo!("Return validator gRPC address from ports")
    }
    
    fn network(&self) -> &Network {
        &self.network
    }

    async fn generate_blocks(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement block generation using local_net")
    }

    async fn generate_blocks_with_delay(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement block generation with delays for sync")
    }

    async fn wait_for_validator_ready(&self) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement validator readiness check")
    }

    async fn close(&mut self) {
        todo!("Implement validator, indexer, and JSON server cleanup")
    }
}

// Conditional implementation of WithClients only when clients are present
impl WithClients for JsonServerTestManager {
    fn clients(&self) -> &Clients {
        self.clients.as_ref().expect("JsonServerTestManager was not configured with clients")
    }

    async fn sync_clients(&self) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement client synchronization")
    }

    async fn get_faucet_address(&self, addr_type: &str) -> String {
        todo!("Implement faucet address generation")
    }

    async fn get_recipient_address(&self, addr_type: &str) -> String {
        todo!("Implement recipient address generation")
    }

    async fn prepare_for_shielding(&self, blocks: u32) -> Result<(), Box<dyn std::error::Error>>
    where 
        Self: WithValidator 
    {
        todo!("Implement prepare_for_shielding workflow")
    }
}

impl WithIndexer for JsonServerTestManager {
    fn indexer_config(&self) -> &IndexerConfig {
        &self.indexer_config
    }

    fn zaino_grpc_address(&self) -> Option<SocketAddr> {
        todo!("Return zaino gRPC address if configured")
    }

    fn zaino_json_address(&self) -> Option<SocketAddr> {
        todo!("Return zaino JSON address if configured")
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
            enable_cookie_auth: true,  // Common for JSON server tests
            enable_clients: false,     // Optional for JSON server tests
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
            JsonRpcAuthConfig::Cookie(todo!("Generate ephemeral cookie path"))
        } else {
            JsonRpcAuthConfig::None
        };

        JsonServerTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
            indexer: todo!("Create default IndexerConfig for JSON server tests"),
            json_auth,
            enable_clients: self.enable_clients,
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        todo!("Launch JsonServerTestManager from builder configuration")
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