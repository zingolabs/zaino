//! Wallet test manager for validator + indexer + clients tests.
//!
//! This manager provides a complete wallet testing environment with validator,
//! indexer, and guaranteed lightclient availability. No Option unwrapping needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use zaino_commons::config::{IndexerConfig, Network};
use zainodlib::error::IndexerError;
use crate::{
    validator::{LocalNet, ValidatorKind},
    ports::TestPorts,
    clients::Clients,
    config::{WalletTestConfig, TestConfig},
    manager::{
        traits::{WithValidator, WithClients, WithIndexer, WithServiceFactories, ConfigurableBuilder, TestConfiguration},
        factories::{FetchServiceBuilder, StateServiceBuilder, BlockCacheBuilder},
    },
};
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;

/// Test manager for wallet tests (validator + indexer + clients).
///
/// This manager guarantees the availability of lightclients, eliminating
/// the need for Option unwrapping in wallet test code.
#[derive(Debug)]
pub struct WalletTestManager {
    local_net: LocalNet,
    ports: TestPorts,
    network: Network,
    indexer_config: IndexerConfig,
    indexer_handle: JoinHandle<Result<(), IndexerError>>,
    clients: Clients, // Always present, not Option!
}

impl WithValidator for WalletTestManager {
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
        todo!("Implement validator and indexer cleanup")
    }
}

impl WithClients for WalletTestManager {
    fn clients(&self) -> &Clients {
        &self.clients
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

impl WithIndexer for WalletTestManager {
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
        todo!("Return JSON server cookie dir if configured")
    }

    fn indexer_handle(&self) -> &JoinHandle<Result<(), IndexerError>> {
        &self.indexer_handle
    }
}

impl WithServiceFactories for WalletTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        todo!("Create pre-configured FetchServiceBuilder")
    }

    fn create_state_service(&self) -> StateServiceBuilder {
        todo!("Create pre-configured StateServiceBuilder")
    }

    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        todo!("Create authenticated JSON-RPC connector")
    }

    fn create_block_cache(&self) -> BlockCacheBuilder {
        todo!("Create pre-configured BlockCacheBuilder")
    }
}

/// Builder for WalletTestManager.
#[derive(Debug, Clone)]
pub struct WalletTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
    enable_clients: bool,
}

impl Default for WalletTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
            enable_clients: true, // Usually true for wallet tests
        }
    }
}

impl WalletTestsBuilder {
    /// Disable lightclients (unusual for wallet tests).
    pub fn no_clients(mut self) -> Self {
        self.enable_clients = false;
        self
    }
}

impl ConfigurableBuilder for WalletTestsBuilder {
    type Manager = WalletTestManager;
    type Config = WalletTestConfig;

    fn build_config(&self) -> Self::Config {
        WalletTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
            indexer: todo!("Create default IndexerConfig for wallet tests"),
            enable_clients: self.enable_clients,
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        todo!("Launch WalletTestManager from builder configuration")
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