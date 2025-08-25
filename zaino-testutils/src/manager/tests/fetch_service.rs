//! FetchService test manager for fetch_service.rs integration tests.
//!
//! **Purpose**: Test standalone FetchService functionality
//! **Scope**: Validator + FetchService + Optional Clients
//! **Use Case**: When testing individual FetchService operations without comparison
//!
//! This manager provides components and methods specifically designed for the fetch_service.rs
//! integration test suite, which validates FetchService functionality in isolation.

use crate::{
    clients::Clients,
    config::{FetchServiceTestConfig, TestConfig},
    manager::{
        factories::FetchServiceBuilder,
        traits::{
            ConfigurableBuilder, LaunchManager, WithClients, WithServiceFactories, WithValidator,
        },
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_state::{FetchService, FetchServiceSubscriber};

/// Test manager for fetch_service.rs integration tests.
///
/// **Purpose**: Test standalone FetchService functionality
/// **Scope**:
/// - Validator (Zebra or Zcashd)
/// - FetchService with configurable sync/db settings
/// - Optional: Wallet clients for transaction testing
///
/// **Use Case**: When you need to test FetchService operations in isolation,
/// without comparison to other services.
///
/// **Components**:
/// - Validator: Configurable (Zebra/Zcashd)
/// - FetchService: Single service with flexible configuration
/// - Optional clients: Faucet + recipient for transaction testing
///
/// **Example Usage**:
/// ```rust
/// // Basic FetchService test without clients
/// let manager = FetchServiceTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .launch().await?;
///
/// let (fetch_service, subscriber) = manager.create_fetch_service_configured(true, true, false).await?;
///
/// // With wallet clients for transaction testing
/// let manager = FetchServiceTestsBuilder::default()
///     .validator(ValidatorKind::Zcashd)
///     .with_clients(true)
///     .launch().await?;
///
/// let clients = manager.clients(); // No Option unwrapping needed
/// ```
#[derive(Debug)]
pub struct FetchServiceTestManager {
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

impl WithValidator for FetchServiceTestManager {
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

impl WithClients for FetchServiceTestManager {
    fn clients(&self) -> &Clients {
        self.clients
            .as_ref()
            .expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }

    fn clients_mut(&mut self) -> &mut Clients {
        self.clients
            .as_mut()
            .expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }
}

impl WithServiceFactories for FetchServiceTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        FetchServiceBuilder::new()
            .with_validator_address(self.validator_rpc_address())
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone())
    }

    fn create_state_service(&self) -> crate::manager::factories::StateServiceBuilder {
        // FetchService tests don't typically need StateService, but provide for completeness
        crate::manager::factories::StateServiceBuilder::new()
            .with_validator_rpc_address(self.validator_rpc_address())
            .with_validator_grpc_address(self.validator_grpc_address())
            .with_network(self.network.clone())
            .with_cache_dir(self.ports.zaino_db.clone())
    }

    fn create_json_connector(
        &self,
    ) -> Result<zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector, Box<dyn std::error::Error>>
    {
        use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;

        let url = format!("http://{}", self.validator_rpc_address()).parse()?;
        let connector = JsonRpSeeConnector::new(url, None)?; // No auth for test validators
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
            self.ports.zaino_db.clone(),
        )
    }
}

impl FetchServiceTestManager {
    /// Create FetchService with specific sync/db configuration.
    ///
    /// This is the primary method for fetch_service.rs tests - it returns a configured
    /// FetchService that matches the common test patterns in the suite.
    ///
    /// **Parameters:**
    /// - `enable_sync`: Whether to enable chain synchronization
    /// - `enable_db`: Whether to enable database persistence  
    /// - `no_sync`: Convenience parameter (inverse of enable_sync)
    ///
    /// Returns: (FetchService, FetchServiceSubscriber)
    pub async fn create_fetch_service_configured(
        &self,
        // todo!: have this take in a Debug struct instead of boolean-traps
        enable_sync: bool,
        enable_db: bool,
        no_sync: bool,
    ) -> Result<(FetchService, FetchServiceSubscriber), Box<dyn std::error::Error>> {
        let actual_sync = enable_sync && !no_sync;

        let (fetch_service, fetch_indexer_subscriber) = self
            .create_fetch_service()
            .enable_sync(actual_sync)
            .with_db(enable_db)
            .build()
            .await?;

        let fetch_subscriber = fetch_indexer_subscriber.inner();

        Ok((fetch_service, fetch_subscriber))
    }

    /// Create FetchService using the common pattern from fetch_service.rs tests.
    ///
    /// This matches the original `create_test_manager_and_fetch_service()` signature
    /// but uses the new builder pattern under the hood.
    ///
    /// **Parameters:**
    /// - `enable_zaino`: Whether to enable zaino processing
    /// - `zaino_no_sync`: Whether to disable sync (inverse logic)
    /// - `zaino_no_db`: Whether to disable database
    ///
    /// Returns: (FetchService, FetchServiceSubscriber)
    pub async fn create_fetch_service_legacy_pattern(
        &self,
        enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
    ) -> Result<(FetchService, FetchServiceSubscriber), Box<dyn std::error::Error>> {
        self.create_fetch_service_configured(
            !zaino_no_sync && enable_zaino,
            !zaino_no_db,
            zaino_no_sync,
        )
        .await
    }

    /// Launch basic FetchService for simple connectivity tests.
    ///
    /// This provides the most basic FetchService setup for tests that just need
    /// to verify basic functionality and connectivity.
    pub async fn launch_basic_fetch_service(
        &self,
    ) -> Result<(FetchService, FetchServiceSubscriber), Box<dyn std::error::Error>> {
        self.create_fetch_service_configured(false, true, true)
            .await
    }

    /// Check if clients are available (useful for conditional client operations).
    pub fn has_clients(&self) -> bool {
        self.clients.is_some()
    }

    /// Common test pattern: Generate blocks and sync clients if available.
    ///
    /// This combines the pattern seen in fetch_service.rs tests.
    pub async fn prepare_for_testing(
        &mut self,
        blocks: u32,
    ) -> Result<(), Box<dyn std::error::Error>>
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
}

/// Builder for FetchServiceTestManager.
#[derive(Debug, Clone)]
pub struct FetchServiceTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
    with_clients: bool,
}

impl Default for FetchServiceTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
            with_clients: false,
        }
    }
}

impl ConfigurableBuilder for FetchServiceTestsBuilder {
    type Manager = FetchServiceTestManager;
    type Config = FetchServiceTestConfig;

    fn build_config(&self) -> Self::Config {
        FetchServiceTestConfig {
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

impl FetchServiceTestsBuilder {
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
