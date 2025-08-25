//! Service creation factories with sensible defaults.
//!
//! This module provides builders for common services that eliminate the massive
//! boilerplate typically required in integration tests. Each builder comes
//! pre-configured with sensible defaults and allows customization as needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_fetch::config::FetchServiceConfig;
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_state::{
    bench::{BlockCache, BlockCacheSubscriber},
    BlockCacheConfig, FetchService, FetchServiceSubscriber, IndexerSubscriber, StateService,
    StateServiceSubscriber, ZcashService,
};

/// Builder for FetchService with sensible defaults.
///
/// Eliminates the 40+ lines of boilerplate typically needed to create a FetchService
/// by providing pre-configured defaults and a clean builder API.
pub struct FetchServiceBuilder {
    validator_address: SocketAddr,
    network: Network,
    enable_sync: bool,
    enable_db: bool,
    auth_enabled: bool,
    data_dir: PathBuf,
}

impl FetchServiceBuilder {
    /// Create a new FetchServiceBuilder with sensible defaults.
    /// 
    /// **Defaults:**
    /// - Validator: localhost:8232 (typical test validator port)
    /// - Network: Regtest (most common for testing)
    /// - Sync: Enabled
    /// - Database: Enabled  
    /// - Auth: Disabled (typical for test environments)
    /// - Data dir: Temporary directory
    pub fn new() -> Self {
        Self {
            validator_address: "127.0.0.1:8232".parse().unwrap(),
            network: zaino_commons::config::Network::Regtest,
            enable_sync: true,
            enable_db: true,
            auth_enabled: false,
            data_dir: std::env::temp_dir().join("zaino-test-fetch"),
        }
    }

    /// Set the validator address to connect to.
    pub fn with_validator_address(mut self, address: SocketAddr) -> Self {
        self.validator_address = address;
        self
    }

    /// Set the network type.
    pub fn with_network(mut self, network: zaino_commons::config::Network) -> Self {
        self.network = network;
        self
    }

    /// Set the data directory for the service.
    pub fn with_data_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.data_dir = dir;
        self
    }

    /// Enable or disable sync functionality.
    pub fn with_sync(mut self, enable: bool) -> Self {
        self.enable_sync = enable;
        self
    }

    /// Enable or disable database functionality.
    pub fn with_db(mut self, enable: bool) -> Self {
        self.enable_db = enable;
        self
    }

    /// Enable authentication.
    pub fn with_auth(mut self) -> Self {
        self.auth_enabled = true;
        self
    }

    /// Disable authentication (convenience method).
    pub fn disable_auth(mut self) -> Self {
        self.auth_enabled = false;
        self
    }

    /// Enable cookie-based authentication with cookie directory path.
    pub fn enable_cookie_auth(mut self, _cookie_dir: String) -> Self {
        // TODO: Implement cookie auth configuration
        self.auth_enabled = true;
        self
    }

    /// Enable sync (convenience method with clearer naming).
    pub fn enable_sync(self, enable: bool) -> Self {
        self.with_sync(enable)
    }

    /// Build the final FetchService and subscriber.
    pub async fn build(
        self,
    ) -> Result<(FetchService, IndexerSubscriber<FetchServiceSubscriber>), Box<dyn std::error::Error>>
    {
        use zaino_commons::config::{
            DebugConfig, JsonRpcValidatorConfig, PasswordAuth, ServiceConfig, StorageConfig,
            ZcashdAuth,
        };

        // Create FetchServiceConfig directly - no IndexerConfig needed!
        let fetch_config = FetchServiceConfig {
            validator: JsonRpcValidatorConfig::Zcashd {
                rpc_address: self.validator_address,
                auth: if self.auth_enabled {
                    ZcashdAuth::Password(PasswordAuth {
                        username: "user".to_string(),
                        password: "pass".to_string(),
                    })
                } else {
                    ZcashdAuth::Disabled
                },
            },
            service: ServiceConfig::default(),
            storage: StorageConfig {
                cache: Default::default(),
                database: zaino_commons::config::DatabaseConfig {
                    path: self.data_dir.clone(),
                    ..Default::default()
                },
            },
            network: self.network,
            debug: DebugConfig {
                no_sync: !self.enable_sync,
                no_db: !self.enable_db,
                slow_sync: false,
            },
        };

        use zaino_state::ZcashService;

        let service = FetchService::spawn(fetch_config).await?;
        let subscriber = service.get_subscriber();

        Ok((service, subscriber))
    }
}

/// Builder for StateService with performance-optimized defaults.
///
/// Creates StateService with zebra-state configuration optimized for test environments.
pub struct StateServiceBuilder {
    validator_rpc_address: SocketAddr,
    validator_grpc_address: SocketAddr,
    network: Network,
    cache_dir: PathBuf,
    ephemeral: bool,
}

impl StateServiceBuilder {
    /// Create a new StateServiceBuilder with sensible defaults.
    /// 
    /// **Defaults:**
    /// - Validator RPC: localhost:8232 (typical test validator RPC port)
    /// - Validator gRPC: localhost:8233 (typical test validator gRPC port)
    /// - Network: Regtest (most common for testing)
    /// - Cache dir: Temporary directory
    /// - Ephemeral: false (persistent state by default)
    pub fn new() -> Self {
        Self {
            validator_rpc_address: "127.0.0.1:8232".parse().unwrap(),
            validator_grpc_address: "127.0.0.1:8233".parse().unwrap(),
            network: zaino_commons::config::Network::Regtest,
            cache_dir: std::env::temp_dir().join("zaino-test-state"),
            ephemeral: false,
        }
    }

    /// Set the validator RPC address to connect to.
    pub fn with_validator_rpc_address(mut self, address: SocketAddr) -> Self {
        self.validator_rpc_address = address;
        self
    }

    /// Set the validator gRPC address to connect to.
    pub fn with_validator_grpc_address(mut self, address: SocketAddr) -> Self {
        self.validator_grpc_address = address;
        self
    }

    /// Set the network type.
    pub fn with_network(mut self, network: zaino_commons::config::Network) -> Self {
        self.network = network;
        self
    }

    /// Set the cache directory for zebra-state.
    pub fn with_cache_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    /// Set the chain cache directory (alias for with_cache_dir).
    pub fn with_chain_cache(self, dir: std::path::PathBuf) -> Self {
        self.with_cache_dir(dir)
    }

    /// Use ephemeral (in-memory) state.
    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Build the final StateService.
    pub async fn build(
        self,
    ) -> Result<(StateService, StateServiceSubscriber), Box<dyn std::error::Error>> {
        use zaino_commons::config::{
            DatabaseConfig, DebugConfig, ServiceConfig, StorageConfig, ZebraStateConfig,
            ZebradAuth, ZebradStateConfig,
        };
        use zaino_state::StateServiceConfig;

        // Create StateServiceConfig directly - no IndexerConfig needed!
        let state_config = StateServiceConfig {
            zebra: ZebradStateConfig {
                rpc_address: self.validator_rpc_address,
                auth: ZebradAuth::Disabled, // Usually disabled for local testing
                state: ZebraStateConfig {
                    cache_dir: self.cache_dir.clone(),
                    ephemeral: self.ephemeral,
                    ..Default::default()
                },
                indexer_rpc_address: self.validator_grpc_address,
                database: DatabaseConfig::default(),
            },
            service: ServiceConfig::default(),
            storage: StorageConfig {
                cache: Default::default(),
                database: zaino_commons::config::DatabaseConfig {
                    path: self.cache_dir.clone(),
                    ..Default::default()
                },
            },
            network: self.network,
            debug: DebugConfig {
                no_sync: false,
                no_db: self.ephemeral,
                slow_sync: false, // Disable slow sync for tests
            },
        };

        let service = StateService::spawn(state_config).await?;
        let subscriber = service.get_subscriber().inner();

        Ok((service, subscriber))
    }
}

/// Builder for BlockCache with performance defaults.
///
/// Creates BlockCache with settings optimized for test performance and reliability.
pub struct BlockCacheBuilder {
    connector: JsonRpSeeConnector,
    network: Network,
    db_path: PathBuf,
    no_sync: bool,
    no_db: bool,
}

impl BlockCacheBuilder {
    /// Create a new BlockCacheBuilder with basic configuration.
    pub fn new(connector: JsonRpSeeConnector, network: Network, db_path: PathBuf) -> Self {
        Self {
            connector,
            network,
            db_path,
            no_sync: false,
            no_db: false,
        }
    }

    /// Disable sync functionality.
    pub fn no_sync(mut self) -> Self {
        self.no_sync = true;
        self
    }

    /// Disable database functionality.
    pub fn no_db(mut self) -> Self {
        self.no_db = true;
        self
    }

    /// Build the final BlockCache and subscriber.
    pub async fn build(
        self,
    ) -> Result<(BlockCache, BlockCacheSubscriber), Box<dyn std::error::Error>> {
        // Create BlockCacheConfig directly
        let cache_config = BlockCacheConfig {
            cache: Default::default(),
            database: zaino_commons::config::DatabaseConfig {
                path: self.db_path.clone(),
                ..Default::default()
            },
            network: self.network,
            no_sync: self.no_sync,
            no_db: self.no_db,
        };

        let cache = BlockCache::spawn(&self.connector, None, cache_config).await?;
        let subscriber = cache.subscriber();

        Ok((cache, subscriber))
    }
}
