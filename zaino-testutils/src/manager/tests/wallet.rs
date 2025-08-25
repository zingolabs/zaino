//! Wallet test manager for validator + indexer + clients tests.
//!
//! This manager provides a complete wallet testing environment with validator,
//! indexer, and guaranteed lightclient availability. No Option unwrapping needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use zaino_commons::config::Network;
use zainodlib::config::IndexerConfig;
use zainodlib::error::IndexerError;
use crate::{
    validator::{LocalNet, ValidatorKind},
    ports::TestPorts,
    clients::Clients,
    config::{WalletTestConfig, TestConfig},
    manager::{
        traits::{WithValidator, WithClients, WithIndexer, WithServiceFactories, ConfigurableBuilder, LaunchManager},
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

impl WithClients for WalletTestManager {
    fn clients(&self) -> &Clients {
        &self.clients
    }
}

impl WithIndexer for WalletTestManager {
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
        None // Wallet tests don't typically use JSON server authentication
    }

    fn indexer_handle(&self) -> &JoinHandle<Result<(), IndexerError>> {
        &self.indexer_handle
    }
}

impl WithServiceFactories for WalletTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        FetchServiceBuilder::new(
            self.validator_rpc_address(),
            self.network.clone(),
            self.ports.zaino_db.clone()
        )
    }

    fn create_state_service(&self) -> StateServiceBuilder {
        StateServiceBuilder::new(
            self.validator_rpc_address(),
            self.validator_grpc_address(),
            self.network.clone(),
            self.ports.zaino_db.clone()
        )
    }

    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
        
        let url = format!("http://{}", self.validator_rpc_address()).parse()?;
        let connector = JsonRpSeeConnector::new(url, None)?; // No auth for test validators
        Ok(connector)
    }

    fn create_block_cache(&self) -> BlockCacheBuilder {
        let connector = self.create_json_connector()
            .expect("Failed to create connector for block cache");
        
        BlockCacheBuilder::new(
            connector,
            self.network.clone(),
            self.ports.zaino_db.clone()
        )
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
            indexer: IndexerConfig::default(),
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