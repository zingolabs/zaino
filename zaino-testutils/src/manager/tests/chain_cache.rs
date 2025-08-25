//! ChainCache test manager for chain_cache.rs integration tests.
//!
//! **Purpose**: Test chain caching via JSON-RPC connector
//! **Scope**: Validator + JsonRpSeeConnector + ChainIndex + Optional Clients
//! **Use Case**: When testing chain caching functionality through JSON-RPC connections
//!
//! This manager provides components and methods specifically designed for the chain_cache.rs
//! integration test suite, which validates chain caching functionality via JSON-RPC connectors.

use crate::{
    config::{ChainCacheTestConfig, TestConfig},
    manager::{
        factories::FetchServiceBuilder,
        traits::{ConfigurableBuilder, LaunchManager, WithClients, WithServiceFactories, WithValidator},
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
    clients::Clients,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;

/// Test manager for chain_cache.rs integration tests.
/// 
/// **Purpose**: Test chain caching via JSON-RPC connector
/// **Scope**: 
/// - Validator (Zebra or Zcashd)
/// - JsonRpSeeConnector for RPC communication
/// - Chain caching functionality
/// - Optional: Wallet clients for transaction testing
/// 
/// **Use Case**: When you need to test chain caching functionality through
/// JSON-RPC connections and validate cache behavior.
/// 
/// **Components**:
/// - Validator: Configurable (Zebra/Zcashd) 
/// - JsonRpSeeConnector: For RPC communication with custom auth
/// - Chain cache: Configurable cache directory
/// - Optional clients: Faucet + recipient for transaction testing
///
/// **Example Usage**:
/// ```rust
/// // Basic chain cache test without clients
/// let manager = ChainCacheTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .launch().await?;
/// 
/// let connector = manager.create_json_connector_with_auth("user", "pass").await?;
/// 
/// // With chain cache directory
/// let manager = ChainCacheTestsBuilder::default()
///     .validator(ValidatorKind::Zcashd)
///     .chain_cache(cache_path)
///     .with_clients(true)
///     .launch().await?;
/// ```
#[derive(Debug)]
pub struct ChainCacheTestManager {
    /// Local validator network
    pub local_net: LocalNet,
    /// Test ports and directories
    pub ports: TestPorts,
    /// Network configuration
    pub network: Network,
    /// Optional chain cache directory
    pub chain_cache: Option<PathBuf>,
    /// Optional wallet clients
    pub clients: Option<Clients>,
}

impl WithValidator for ChainCacheTestManager {
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
}

impl WithClients for ChainCacheTestManager {
    fn clients(&self) -> &Clients {
        self.clients.as_ref().expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }

    fn clients_mut(&mut self) -> &mut Clients {
        self.clients.as_mut().expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }
}

impl WithServiceFactories for ChainCacheTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        FetchServiceBuilder::new()
            .with_validator_address(self.validator_rpc_address())
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone())
    }

    fn create_state_service(&self) -> crate::manager::factories::StateServiceBuilder {
        crate::manager::factories::StateServiceBuilder::new()
            .with_validator_rpc_address(self.validator_rpc_address())
            .with_validator_grpc_address(self.validator_grpc_address())
            .with_network(self.network.clone())
            .with_cache_dir(self.chain_cache.clone().unwrap_or_else(|| self.ports.zaino_db.clone()))
    }

    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        let url = format!("http://{}", self.validator_rpc_address()).parse()?;
        let connector = JsonRpSeeConnector::new(url, None)?; // No auth for basic connector
        Ok(connector)
    }

    fn create_block_cache(&self) -> crate::manager::factories::BlockCacheBuilder {
        // Create a basic connector for the cache (we need it for initialization)
        let connector = self
            .create_json_connector()
            .expect("Failed to create connector for block cache");

        crate::manager::factories::BlockCacheBuilder::new(
            connector, 
            self.network.clone(), 
            self.ports.zaino_db.clone()
        )
    }
}

impl ChainCacheTestManager {
    /// Create JsonRpSeeConnector with authentication.
    /// 
    /// This matches the pattern used in chain_cache.rs tests where authentication
    /// is configured. For now, we'll use the basic connector without auth.
    /// 
    /// **Parameters:**
    /// - `username`: Authentication username (currently ignored)
    /// - `password`: Authentication password (currently ignored)
    /// 
    /// Returns: JsonRpSeeConnector configured for chain cache testing
    pub async fn create_json_connector_with_auth(
        &self,
        _username: &str,
        _password: &str,
    ) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        // For now, use basic connector without auth
        // TODO: Implement proper basic auth support when needed
        let url = format!("http://{}", self.validator_rpc_address()).parse()?;
        let connector = JsonRpSeeConnector::new(url, None)?;
        
        Ok(connector)
    }

    /// Create JsonRpSeeConnector and TestManager following the chain_cache.rs pattern.
    /// 
    /// This matches the original `create_test_manager_and_connector()` signature
    /// from chain_cache.rs tests.
    /// 
    /// **Parameters:**
    /// - `enable_zaino`: Whether to enable zaino processing
    /// - `zaino_no_sync`: Whether to disable sync 
    /// - `zaino_no_db`: Whether to disable database
    /// 
    /// Returns: JsonRpSeeConnector configured for chain cache testing
    pub async fn create_connector_legacy_pattern(
        &self,
        enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
    ) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        let _enable_zaino = enable_zaino;
        let _zaino_no_sync = zaino_no_sync;
        let _zaino_no_db = zaino_no_db;
        
        // Create connector with test credentials (matching chain_cache.rs pattern)
        self.create_json_connector_with_auth("xxxxxx", "xxxxxx").await
    }

    /// Get the chain cache directory, using the configured one or default.
    pub fn get_chain_cache_dir(&self) -> PathBuf {
        self.chain_cache.clone().unwrap_or_else(|| self.ports.zaino_db.clone())
    }

    /// Check if clients are available (useful for conditional client operations).
    pub fn has_clients(&self) -> bool {
        self.clients.is_some()
    }

    /// Common test pattern: Generate blocks and setup for chain cache testing.
    /// 
    /// This combines the pattern seen in chain_cache.rs tests.
    pub async fn prepare_for_cache_testing(&mut self, blocks: u32) -> Result<(), Box<dyn std::error::Error>>
    where
        Self: WithClients,
    {
        // Generate initial blocks
        self.generate_blocks_with_delay(blocks).await?;
        
        // Sync clients if available
        if self.has_clients() {
            self.faucet().sync_and_await().await?;
        }
        
        Ok(())
    }

    /// Create a StateService for chain index testing (used in some chain_cache.rs tests).
    pub async fn create_state_service_for_cache(&self) -> Result<zaino_state::StateService, Box<dyn std::error::Error>> {
        let (state_service, _) = self.create_state_service().build().await?;
        Ok(state_service)
    }
}

/// Builder for ChainCacheTestManager.
#[derive(Debug, Clone)]
pub struct ChainCacheTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
    with_clients: bool,
}

impl Default for ChainCacheTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
            with_clients: false,
        }
    }
}

impl ConfigurableBuilder for ChainCacheTestsBuilder {
    type Manager = ChainCacheTestManager;
    type Config = ChainCacheTestConfig;

    fn build_config(&self) -> Self::Config {
        ChainCacheTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
            with_clients: self.with_clients,
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

impl ChainCacheTestsBuilder {
    /// Enable wallet clients (faucet + recipient) for transaction testing.
    /// 
    /// When enabled, the manager will implement WithClients trait and provide
    /// access to wallet operations without Option unwrapping.
    pub fn with_clients(mut self, enable: bool) -> Self {
        self.with_clients = enable;
        self
    }

    /// Configure for Zcashd validator.
    pub fn zcashd(mut self) -> Self {
        self.validator_kind = ValidatorKind::Zcashd;
        self
    }

    /// Configure for Zebra validator.
    pub fn zebra(mut self) -> Self {
        self.validator_kind = ValidatorKind::Zebra;
        self
    }

    /// Configure for mainnet testing.
    pub fn mainnet(mut self) -> Self {
        self.network = Network::Mainnet;
        self
    }

    /// Configure for testnet testing.
    pub fn testnet(mut self) -> Self {
        self.network = Network::Testnet;
        self
    }

    /// Configure for regtest (default).
    pub fn regtest(mut self) -> Self {
        self.network = Network::Regtest;
        self
    }
}