//! JsonServerComparison test manager for json_server.rs integration tests.
//!
//! **Purpose**: Test Zaino JSON-RPC server by comparing zcashd vs zaino responses
//! **Scope**: Zcashd Validator + Zaino JSON Server + Dual FetchServices + Optional Clients
//! **Use Case**: When testing that Zaino's JSON-RPC server produces identical responses to zcashd
//!
//! This manager provides components and methods specifically designed for the json_server.rs
//! integration test suite, which validates that Zaino's JSON-RPC server produces identical
//! responses to zcashd for all supported JSON-RPC operations.

use crate::{
    config::{JsonServerComparisonTestConfig, TestConfig},
    manager::{
        factories::FetchServiceBuilder,
        traits::{ConfigurableBuilder, LaunchManager, WithClients, WithValidator},
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
    clients::{ClientAddressType, Clients},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_state::{FetchService, FetchServiceSubscriber};

/// Test manager for json_server.rs integration tests.
/// 
/// **Purpose**: Test Zaino JSON-RPC server behavioral compatibility with zcashd
/// **Scope**: 
/// - Zcashd validator (required for JSON-RPC compatibility testing)
/// - Zaino indexer with JSON-RPC server enabled
/// - FetchService pointed at zcashd (baseline)
/// - FetchService pointed at zaino JSON server (test subject)
/// - Cookie authentication configuration
/// - Optional: Wallet clients for transaction creation
/// 
/// **Use Case**: When you need to verify that Zaino's JSON-RPC server produces identical 
/// responses to zcashd for all supported operations.
/// 
/// **Components**:
/// - Validator: Always Zcashd (required for JSON-RPC baseline)
/// - Indexer: Zaino with JSON-RPC server enabled
/// - Services: Dual FetchServices for comparison testing
/// - Authentication: Cookie-based authentication support
/// - Clients: Optional wallet clients (faucet + recipient) for transaction testing
///
/// **Example Usage**:
/// ```rust
/// // Basic comparison without cookie auth or clients
/// let manager = JsonServerComparisonTestsBuilder::default()
///     .launch().await?;
/// 
/// let (zcashd_service, zcashd_sub, zaino_service, zaino_sub) = 
///     manager.create_comparison_services().await?;
/// 
/// // With cookie authentication
/// let manager = JsonServerComparisonTestsBuilder::default()
///     .enable_cookie_auth(true)
///     .launch().await?;
/// 
/// // With wallet clients  
/// let manager = JsonServerComparisonTestsBuilder::default()
///     .with_clients(true)
///     .launch().await?;
/// 
/// let clients = manager.clients(); // No Option unwrapping needed
/// ```
#[derive(Debug)]
pub struct JsonServerComparisonTestManager {
    pub local_net: LocalNet,
    pub ports: TestPorts,
    pub network: Network,
    pub chain_cache: Option<PathBuf>,
    pub cookie_auth_enabled: bool,
    pub clients: Option<Clients>,
    /// Directory for storing cookie files (when cookie auth is enabled).
    pub cookie_dir: Option<PathBuf>,
}

impl WithValidator for JsonServerComparisonTestManager {
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

impl WithClients for JsonServerComparisonTestManager {
    fn clients(&self) -> &Clients {
        self.clients.as_ref().expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }

    fn clients_mut(&mut self) -> &mut Clients {
        self.clients.as_mut().expect("Clients not enabled for this manager. Use with_clients(true) in builder.")
    }
}

impl JsonServerComparisonTestManager {
    /// Create both zcashd and zaino FetchServices for JSON-RPC response comparison.
    /// 
    /// This is the primary method for json_server.rs tests - it returns:
    /// - FetchService connected to zcashd (baseline responses)
    /// - FetchService connected to zaino JSON server (test responses)  
    /// 
    /// Both services are configured identically except for their target endpoint.
    /// 
    /// Returns: (ZcashdFetchService, ZcashdSubscriber, ZainoFetchService, ZainoSubscriber)
    pub async fn create_comparison_services(&self) -> Result<
        (FetchService, FetchServiceSubscriber, FetchService, FetchServiceSubscriber), 
        Box<dyn std::error::Error>
    > {
        // Create FetchService pointing to zcashd
        let zcashd_fetch_service = self.create_zcashd_fetch_service().await?;
        let zcashd_subscriber = zcashd_fetch_service.get_subscriber().inner();

        // Wait for services to initialize
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // Create FetchService pointing to zaino JSON server  
        let zaino_fetch_service = self.create_zaino_fetch_service().await?;
        let zaino_subscriber = zaino_fetch_service.get_subscriber().inner();

        // Additional initialization delay
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok((zcashd_fetch_service, zcashd_subscriber, zaino_fetch_service, zaino_subscriber))
    }

    /// Create FetchService connected to the zcashd validator.
    /// 
    /// This provides the "baseline" responses that zaino should match.
    pub async fn create_zcashd_fetch_service(&self) -> Result<FetchService, Box<dyn std::error::Error>> {
        let fetch_service = FetchServiceBuilder::new()
            .with_validator_address(self.validator_rpc_address())
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone())
            .disable_auth() // No auth for test zcashd
            .enable_sync(true)
            .build()
            .await?;

        Ok(fetch_service.0)
    }

    /// Create FetchService connected to the zaino JSON-RPC server.
    /// 
    /// This provides the "test" responses that should match zcashd.
    pub async fn create_zaino_fetch_service(&self) -> Result<FetchService, Box<dyn std::error::Error>> {
        let zaino_json_address = self.get_zaino_json_server_address()?;
        
        let mut builder = FetchServiceBuilder::new()
            .with_validator_address(zaino_json_address)
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone());

        // Configure authentication if enabled
        if self.cookie_auth_enabled {
            if let Some(cookie_dir) = &self.cookie_dir {
                builder = builder.enable_cookie_auth(cookie_dir.to_string_lossy().to_string());
            }
        } else {
            builder = builder.disable_auth();
        }

        let fetch_service = builder
            .enable_sync(true)
            .build()
            .await?;

        Ok(fetch_service.0)
    }

    /// Get the zaino JSON-RPC server address.
    /// 
    /// This address is where the zaino indexer exposes its JSON-RPC API.
    pub fn get_zaino_json_server_address(&self) -> Result<SocketAddr, Box<dyn std::error::Error>> {
        self.ports.zaino_json
            .ok_or_else(|| "Zaino JSON server address not configured".into())
    }

    /// Check if cookie authentication is enabled.
    pub fn is_cookie_auth_enabled(&self) -> bool {
        self.cookie_auth_enabled
    }

    /// Get the cookie directory path (if cookie auth is enabled).
    pub fn cookie_directory(&self) -> Option<&PathBuf> {
        self.cookie_dir.as_ref()
    }

    /// Check if clients are available (useful for conditional client operations).
    pub fn has_clients(&self) -> bool {
        self.clients.is_some()
    }

    /// Wait for both zcashd and zaino services to be ready with startup delays.
    /// 
    /// This handles the timing requirements seen in json_server.rs tests.
    pub async fn wait_for_services_ready(&self) -> Result<(), Box<dyn std::error::Error>> {
        println!("Launching test manager..");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        
        println!("Launching zcashd fetch service..");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        
        println!("Launching zaino fetch service..");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        
        println!("Testmanager launch complete!");
        Ok(())
    }
}

/// Builder for JsonServerComparisonTestManager.
#[derive(Debug, Clone)]
pub struct JsonServerComparisonTestsBuilder {
    network: Network,
    chain_cache: Option<PathBuf>,
    enable_cookie_auth: bool,
    with_clients: bool,
}

impl Default for JsonServerComparisonTestsBuilder {
    fn default() -> Self {
        Self {
            network: Network::Regtest,
            chain_cache: None,
            enable_cookie_auth: false,
            with_clients: false,
        }
    }
}

impl ConfigurableBuilder for JsonServerComparisonTestsBuilder {
    type Manager = JsonServerComparisonTestManager;
    type Config = JsonServerComparisonTestConfig;

    fn build_config(&self) -> Self::Config {
        JsonServerComparisonTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: ValidatorKind::Zcashd, // Always zcashd for JSON-RPC compatibility
                chain_cache: self.chain_cache.clone(),
            },
            enable_cookie_auth: self.enable_cookie_auth,
            with_clients: self.with_clients,
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        let config = self.build_config();
        config.launch_manager().await
    }

    fn validator(self, _kind: ValidatorKind) -> Self {
        // JSON server tests always use zcashd - ignore the parameter
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

impl JsonServerComparisonTestsBuilder {
    /// Enable cookie-based authentication for zaino JSON-RPC server.
    /// 
    /// When enabled, the zaino FetchService will be configured to use cookie
    /// authentication to connect to the zaino JSON server.
    pub fn enable_cookie_auth(mut self, enable: bool) -> Self {
        self.enable_cookie_auth = enable;
        self
    }

    /// Enable wallet clients (faucet + recipient) for transaction testing.
    /// 
    /// When enabled, the manager will implement WithClients trait and provide
    /// access to wallet operations without Option unwrapping.
    pub fn with_clients(mut self, enable: bool) -> Self {
        self.with_clients = enable;
        self
    }

    /// Configure for regtest with all network upgrades active.
    pub fn for_regtest(mut self) -> Self {
        self.network = Network::new_regtest(
            zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                before_overwinter: Some(1),
                overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(1),
                nu6: Some(1),
                nu6_1: None,
                nu7: None,
            },
        );
        self
    }
}