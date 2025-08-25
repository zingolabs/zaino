//! StateServiceComparison test manager for state_service.rs integration tests.
//!
//! **Purpose**: Compare FetchService vs StateService responses for behavioral compatibility
//! **Scope**: Validator + FetchService + StateService + Optional Clients
//! **Use Case**: When testing that StateService produces identical responses to FetchService
//!
//! This manager provides components and methods specifically designed for the state_service.rs
//! integration test suite, which validates that StateService (zebra-state backend) produces
//! identical responses to FetchService (JSON-RPC backend) for all supported operations.

use crate::{
    config::{StateServiceComparisonTestConfig, TestConfig},
    manager::{
        factories::{FetchServiceBuilder, StateServiceBuilder},
        traits::{ConfigurableBuilder, LaunchManager, WithClients, WithServiceFactories, WithValidator},
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
    clients::Clients,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_state::{FetchService, FetchServiceSubscriber, StateService, StateServiceSubscriber};

/// Test manager for state_service.rs integration tests.
/// 
/// **Purpose**: Compare FetchService vs StateService behavioral compatibility
/// **Scope**: 
/// - Validator (Zebra or Zcashd)
/// - FetchService (connected to validator via JSON-RPC)
/// - StateService (connected to validator via zebra-state)
/// - Optional: Wallet clients for transaction creation
/// 
/// **Use Case**: When you need to verify that StateService produces identical 
/// responses to FetchService for all supported JSON-RPC operations.
/// 
/// **Components**:
/// - Validator: Configurable (Zebra/Zcashd) with custom network parameters
/// - Services: Both FetchService and StateService with matching configurations
/// - Clients: Optional wallet clients (faucet + recipient) for transaction testing
///
/// **Example Usage**:
/// ```rust
/// // For tests without wallet clients
/// let manager = StateServiceComparisonTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .network(Network::Regtest) 
///     .launch().await?;
/// 
/// let (fetch_service, fetch_sub, state_service, state_sub) = 
///     manager.create_dual_services().await?;
/// 
/// // For tests with wallet clients  
/// let manager = StateServiceComparisonTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .network(Network::Regtest)
///     .with_clients(true)
///     .launch().await?;
/// 
/// let clients = manager.clients(); // No Option unwrapping needed
/// ```
#[derive(Debug)]
pub struct StateServiceComparisonTestManager {
    pub local_net: LocalNet,
    pub ports: TestPorts,
    pub network: Network,
    pub chain_cache: Option<PathBuf>,
    pub clients: Option<Clients>,
}

impl WithValidator for StateServiceComparisonTestManager {
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

impl WithClients for StateServiceComparisonTestManager {
    fn clients(&self) -> &Clients {
        self.clients.as_ref().expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }

    fn clients_mut(&mut self) -> &mut Clients {
        self.clients.as_mut().expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }
}

impl WithServiceFactories for StateServiceComparisonTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        FetchServiceBuilder::new()
            .with_validator_address(self.validator_rpc_address())
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone())
    }

    fn create_state_service(&self) -> StateServiceBuilder {
        StateServiceBuilder::new()
            .with_validator_rpc_address(self.validator_rpc_address())
            .with_validator_grpc_address(self.validator_grpc_address())
            .with_network(self.network.clone())
            .with_chain_cache(self.chain_cache.clone().unwrap_or_else(|| self.ports.zaino_db.clone()))
    }

    fn create_json_connector(&self) -> Result<zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector, Box<dyn std::error::Error>> {
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
            self.ports.zaino_db.clone()
        )
    }
}

impl StateServiceComparisonTestManager {
    /// Create both FetchService and StateService with matching configurations.
    /// 
    /// This is the primary method for state_service.rs tests - it returns both
    /// services configured identically except for their backend (JSON-RPC vs zebra-state).
    /// 
    /// Returns: (FetchService, FetchServiceSubscriber, StateService, StateServiceSubscriber)
    pub async fn create_dual_services(&self) -> Result<
        (FetchService, FetchServiceSubscriber, StateService, StateServiceSubscriber), 
        Box<dyn std::error::Error>
    > {
        // Create FetchService 
        let (fetch_service, fetch_indexer_subscriber) = self
            .create_fetch_service()
            .build()
            .await?;

        let fetch_subscriber = fetch_indexer_subscriber.inner();

        // Create StateService with matching configuration
        let (state_service, state_subscriber) = self
            .create_state_service()
            .build()
            .await?;

        // Allow services to initialize
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok((fetch_service, fetch_subscriber, state_service, state_subscriber))
    }

    /// Create dual services with custom network configuration for mainnet/testnet tests.
    /// 
    /// This method handles the special network configuration logic needed for 
    /// mainnet and testnet tests, including startup delays and sync settings.
    pub async fn create_dual_services_with_network(&self, enable_sync: bool) -> Result<
        (FetchService, FetchServiceSubscriber, StateService, StateServiceSubscriber), 
        Box<dyn std::error::Error>
    > {
        // For mainnet/testnet, add startup delay
        if matches!(self.network, Network::Mainnet | Network::Testnet) {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
        }

        // Create FetchService with sync configuration
        let (fetch_service, fetch_indexer_subscriber) = self
            .create_fetch_service()
            .enable_sync(enable_sync)
            .build()
            .await?;

        let fetch_subscriber = fetch_indexer_subscriber.inner();

        // Create StateService 
        let (state_service, state_subscriber) = self
            .create_state_service()
            .build()
            .await?;

        // Allow services to initialize
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok((fetch_service, fetch_subscriber, state_service, state_subscriber))
    }

    /// Common workflow: Generate blocks, sync clients, and shield funds.
    /// 
    /// This combines the common pattern seen in state_service.rs tests of:
    /// 1. Generate blocks for mining rewards
    /// 2. Sync faucet client  
    /// 3. Shield the transparent funds
    /// 4. Generate confirmation block
    /// 5. Final sync
    pub async fn prepare_for_testing(&mut self, blocks: u32) -> Result<(), Box<dyn std::error::Error>>
    where
        Self: WithClients,
    {
        // Generate initial blocks
        if blocks > 100 {
            // Large block generation - do it in chunks with sync
            self.generate_blocks(100).await?;
            self.faucet().sync_and_await().await?;
            self.faucet().quick_shield().await?;
            self.generate_blocks(blocks - 100).await?;
            self.faucet().sync_and_await().await?;
        } else {
            self.generate_blocks(blocks).await?;
        }
        
        self.faucet().sync_and_await().await?;
        Ok(())
    }

    /// Check if clients are available (useful for conditional client operations).
    pub fn has_clients(&self) -> bool {
        self.clients.is_some()
    }
}

/// Builder for StateServiceComparisonTestManager.
#[derive(Debug, Clone)]
pub struct StateServiceComparisonTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
    with_clients: bool,
}

impl Default for StateServiceComparisonTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
            with_clients: false,
        }
    }
}

impl ConfigurableBuilder for StateServiceComparisonTestsBuilder {
    type Manager = StateServiceComparisonTestManager;
    type Config = StateServiceComparisonTestConfig;

    fn build_config(&self) -> Self::Config {
        StateServiceComparisonTestConfig {
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

impl StateServiceComparisonTestsBuilder {
    /// Enable wallet clients (faucet + recipient) for transaction testing.
    /// 
    /// When enabled, the manager will implement WithClients trait and provide
    /// access to wallet operations without Option unwrapping.
    pub fn with_clients(mut self, enable: bool) -> Self {
        self.with_clients = enable;
        self
    }

    /// Configure for mainnet testing with appropriate delays and settings.
    pub fn for_mainnet(mut self) -> Self {
        self.network = Network::Mainnet;
        self
    }

    /// Configure for testnet testing with appropriate delays and settings.
    pub fn for_testnet(mut self) -> Self {
        self.network = Network::Testnet;
        self
    }

    /// Configure for regtest with custom activation heights.
    pub fn for_regtest_with_activations(mut self) -> Self {
        self.network = Network::Regtest;
        self
    }
}