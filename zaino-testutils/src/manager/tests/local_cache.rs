//! LocalCache test manager for local_cache.rs integration tests.
//!
//! **Purpose**: Test local block cache functionality via BlockCache 
//! **Scope**: Validator + JsonRpSeeConnector + BlockCache + BlockCacheSubscriber
//! **Use Case**: When testing block caching, chain height validation, and finalised vs non-finalised state operations
//!
//! This manager provides components and methods specifically designed for the local_cache.rs
//! integration test suite, which validates block caching functionality and state management.

use crate::{
    config::{LocalCacheTestConfig, TestConfig},
    manager::{
        factories::FetchServiceBuilder,
        traits::{ConfigurableBuilder, LaunchManager, WithServiceFactories, WithValidator},
    },
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_commons::config::{BlockCacheConfig, CacheConfig, DatabaseConfig};
use zaino_state::bench::{BlockCache, BlockCacheSubscriber};

/// Test manager for local_cache.rs integration tests.
/// 
/// **Purpose**: Test local block cache functionality via BlockCache
/// **Scope**: 
/// - Validator (Zebra or Zcashd)
/// - JsonRpSeeConnector for RPC communication with basic auth
/// - BlockCache for local caching functionality
/// - BlockCacheSubscriber for chain operations and state management
/// 
/// **Use Case**: When you need to test block caching functionality, chain height 
/// validation, and finalised vs non-finalised state operations.
/// 
/// **Components**:
/// - Validator: Configurable (Zebra/Zcashd) 
/// - JsonRpSeeConnector: For RPC communication with basic auth support
/// - BlockCache: Local block cache with configurable sync/db settings
/// - BlockCacheSubscriber: For chain operations and state queries
///
/// **Example Usage**:
/// ```rust
/// // Basic local cache test without database
/// let manager = LocalCacheTestsBuilder::default()
///     .validator(ValidatorKind::Zcashd)
///     .launch().await?;
/// 
/// let (connector, block_cache, subscriber) = manager.create_block_cache_setup(true, true).await?;
/// 
/// // With database for persistent caching
/// let manager = LocalCacheTestsBuilder::default()
///     .validator(ValidatorKind::Zebra)
///     .launch().await?;
/// 
/// let (connector, block_cache, subscriber) = manager.create_block_cache_setup(false, false).await?;
/// ```
#[derive(Debug)]
pub struct LocalCacheTestManager {
    /// Local validator network
    pub local_net: LocalNet,
    /// Test ports and directories
    pub ports: TestPorts,
    /// Network configuration
    pub network: Network,
    /// Optional chain cache directory
    pub chain_cache: Option<PathBuf>,
}

impl WithValidator for LocalCacheTestManager {
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

impl WithServiceFactories for LocalCacheTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        FetchServiceBuilder::new()
            .with_validator_address(self.validator_rpc_address())
            .with_network(self.network.clone())
            .with_data_dir(self.ports.zaino_db.clone())
    }

    fn create_state_service(&self) -> crate::manager::factories::StateServiceBuilder {
        // Local cache tests don't typically need StateService, but provide for completeness
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

impl LocalCacheTestManager {
    /// Check if the validator node is available and responsive.
    /// 
    /// This performs a health check against the validator RPC endpoint
    /// to ensure it's ready for testing.
    pub async fn check_validator_health(&self) -> Result<(), Box<dyn std::error::Error>> {
        let connector = self.create_json_connector()?;
        
        // Simple health check - try to get blockchain info
        let _info = connector.get_blockchain_info().await
            .map_err(|e| format!("Validator health check failed: {}", e))?;
            
        Ok(())
    }
    
    /// Build the RPC URL for the validator with optional authentication.
    /// 
    /// **Parameters:**
    /// - `username`: Optional authentication username
    /// - `password`: Optional authentication password  
    /// 
    /// Returns: Properly formatted URL for RPC connection
    pub fn build_validator_url(
        &self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<reqwest::Url, Box<dyn std::error::Error>> {
        let base_url = format!("http://{}", self.validator_rpc_address());
        
        let mut url = reqwest::Url::parse(&base_url)?;
        
        // Add basic auth if credentials provided
        if let (Some(user), Some(pass)) = (username, password) {
            url.set_username(user).map_err(|_| "Failed to set username")?;
            url.set_password(Some(pass)).map_err(|_| "Failed to set password")?;
        }
        
        Ok(url)
    }

    /// Create JsonRpSeeConnector with optional basic authentication.
    /// 
    /// **Parameters:**
    /// - `username`: Optional authentication username
    /// - `password`: Optional authentication password  
    /// 
    /// Returns: JsonRpSeeConnector configured for local cache testing
    pub async fn create_json_connector_with_auth(
        &self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        let url = self.build_validator_url(username, password)?;
        
        // TODO: Implement proper basic auth support when JsonRpSeeConnector supports it
        // For now, create connector without auth credentials
        let connector = JsonRpSeeConnector::new(url.to_string().parse()?, None)?;
        
        // Perform health check to ensure connection works
        let _info = connector.get_blockchain_info().await
            .map_err(|e| format!("Failed to connect to validator at {}: {}", url, e))?;
            
        Ok(connector)
    }

    /// Create complete block cache setup following the local_cache.rs pattern.
    /// 
    /// This matches the original `create_test_manager_and_block_cache()` signature
    /// from local_cache.rs tests, returning all components needed for testing.
    /// 
    /// **Parameters:**
    /// - `no_sync`: Whether to disable chain synchronization
    /// - `no_db`: Whether to disable database persistence
    /// 
    /// Returns: (JsonRpSeeConnector, BlockCache, BlockCacheSubscriber)
    pub async fn create_block_cache_setup(
        &self,
        no_sync: bool,
        no_db: bool,
    ) -> Result<(JsonRpSeeConnector, BlockCache, BlockCacheSubscriber), Box<dyn std::error::Error>> {
        // Create authenticated connector (matches original test pattern)
        let json_service = self.create_json_connector_with_auth(Some("xxxxxx"), Some("xxxxxx")).await?;

        // Create BlockCacheConfig with proper structure
        let block_cache_config = BlockCacheConfig {
            cache: CacheConfig {
                shard_amount: Some(4),
                capacity: None,
            },
            database: DatabaseConfig {
                path: self.ports.zaino_db.clone(),
                size: None,
            },
            network: self.network.clone(), // Use zaino Network, not zebra Network
            no_sync,
            no_db,
        };

        // Spawn BlockCache with the configuration
        let block_cache = BlockCache::spawn(&json_service, None, block_cache_config).await?;
        let block_cache_subscriber = block_cache.subscriber();

        Ok((json_service, block_cache, block_cache_subscriber))
    }

    /// Create complete block cache setup using the legacy pattern.
    /// 
    /// This matches the original helper function parameters for backward compatibility.
    /// 
    /// **Parameters:**
    /// - `enable_zaino`: Whether to enable zaino processing (currently ignored)
    /// - `zaino_no_sync`: Whether to disable sync
    /// - `zaino_no_db`: Whether to disable database
    /// 
    /// Returns: (JsonRpSeeConnector, BlockCache, BlockCacheSubscriber)
    pub async fn create_block_cache_legacy_pattern(
        &self,
        _enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
    ) -> Result<(JsonRpSeeConnector, BlockCache, BlockCacheSubscriber), Box<dyn std::error::Error>> {
        self.create_block_cache_setup(zaino_no_sync, zaino_no_db).await
    }


    /// Get the chain cache directory, using the configured one or default.
    pub fn get_chain_cache_dir(&self) -> PathBuf {
        self.chain_cache.clone().unwrap_or_else(|| self.ports.zaino_db.clone())
    }

    /// Common test pattern: Generate block batches and validate chain heights.
    /// 
    /// This combines the pattern seen in local_cache.rs tests for processing
    /// multiple batches of blocks and validating chain state consistency.
    /// 
    /// **Parameters:**
    /// - `batches`: Number of 100-block batches to generate and validate
    /// - `json_service`: JsonRpSeeConnector for validator queries
    /// - `block_cache_subscriber`: BlockCacheSubscriber for cache queries
    /// - `finalised_state_subscriber`: Optional subscriber for finalised state queries
    pub async fn process_block_batches_and_validate(
        &mut self,
        batches: u32,
        json_service: &JsonRpSeeConnector,
        block_cache_subscriber: &mut BlockCacheSubscriber,
        finalised_state_subscriber: Option<&zaino_state::bench::BlockCacheSubscriber>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use zebra_chain::block::Height;
        use zebra_state::HashOrHeight;

        for batch in 1..=batches {
            println!("Processing batch {}/{}", batch, batches);

            // Generate 100 blocks with delays (matches original pattern)
            for height in 1..=100 {
                println!("Generating block at height: {}", height);
                self.generate_blocks_with_delay(1).await?;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

            // Validate chain heights (matches original assertions)
            let validator_height = json_service.get_blockchain_info().await?.blocks.0;
            let non_finalised_state_height = block_cache_subscriber.get_chain_height().await?.0;

            println!("Validator height: {}", validator_height);
            println!("Non-finalised state height: {}", non_finalised_state_height);

            assert_eq!(validator_height, non_finalised_state_height);

            // Fetch blocks from non-finalised state
            let mut non_finalised_state_blocks = Vec::new();
            let start_height = if let Some(finalised_subscriber) = finalised_state_subscriber {
                let finalised_height = finalised_subscriber.get_chain_height().await.unwrap_or(Height(0)).0;
                finalised_height + 1
            } else {
                1
            };

            for height in start_height..=non_finalised_state_height {
                let block = block_cache_subscriber
                    .non_finalised_state
                    .get_compact_block(HashOrHeight::Height(Height(height)))
                    .await?;
                non_finalised_state_blocks.push(block);
            }

            println!(
                "Retrieved {} blocks from non-finalised state",
                non_finalised_state_blocks.len()
            );
        }

        Ok(())
    }

    /// Simple launch pattern for basic cache testing without processing.
    /// 
    /// This matches the `launch_local_cache` helper function pattern.
    pub async fn launch_simple_cache_test(
        &self,
        no_db: bool,
    ) -> Result<BlockCacheSubscriber, Box<dyn std::error::Error>> {
        let (_json_service, _block_cache, block_cache_subscriber) = 
            self.create_block_cache_setup(true, no_db).await?;

        println!("Block cache status: {:?}", block_cache_subscriber.status());
        Ok(block_cache_subscriber)
    }
}

/// Builder for LocalCacheTestManager.
#[derive(Debug, Clone)]
pub struct LocalCacheTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
}

impl Default for LocalCacheTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
        }
    }
}

impl ConfigurableBuilder for LocalCacheTestsBuilder {
    type Manager = LocalCacheTestManager;
    type Config = LocalCacheTestConfig;

    fn build_config(&self) -> Self::Config {
        LocalCacheTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
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

impl LocalCacheTestsBuilder {
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