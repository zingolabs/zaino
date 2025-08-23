//! Service creation factories with sensible defaults.
//!
//! This module provides builders for common services that eliminate the massive
//! boilerplate typically required in integration tests. Each builder comes
//! pre-configured with sensible defaults and allows customization as needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zaino_state::bench::{BlockCache, BlockCacheConfig, BlockCacheSubscriber};
use zaino_fetch::{FetchService, FetchServiceSubscriber, FetchServiceConfig};
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_state::{StateService, StateServiceConfig};

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
    /// Create a new FetchServiceBuilder with basic configuration.
    pub fn new(validator_address: SocketAddr, network: Network, data_dir: PathBuf) -> Self {
        Self {
            validator_address,
            network,
            enable_sync: true,
            enable_db: true, 
            auth_enabled: false,
            data_dir,
        }
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

    /// Build the final FetchService and subscriber.
    pub async fn build(self) -> Result<(FetchService, FetchServiceSubscriber), Box<dyn std::error::Error>> {
        todo!("Implement FetchService creation with configured options")
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
    /// Create a new StateServiceBuilder with basic configuration.
    pub fn new(
        validator_rpc_address: SocketAddr,
        validator_grpc_address: SocketAddr,
        network: Network,
        cache_dir: PathBuf,
    ) -> Self {
        Self {
            validator_rpc_address,
            validator_grpc_address,
            network,
            cache_dir,
            ephemeral: false,
        }
    }

    /// Use ephemeral (in-memory) state.
    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Build the final StateService.
    pub async fn build(self) -> Result<StateService, Box<dyn std::error::Error>> {
        todo!("Implement StateService creation with configured options")
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
    pub async fn build(self) -> Result<(BlockCache, BlockCacheSubscriber), Box<dyn std::error::Error>> {
        todo!("Implement BlockCache creation with configured options")
    }
}