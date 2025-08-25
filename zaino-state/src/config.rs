//! Holds config data for Zaino-State services.

pub use zaino_commons::config::{
    BlockCacheConfig, DebugConfig, Network, ServiceConfig, StorageConfig, ZainodServiceConfig,
    ZebradStateConfig,
};

/// Minimal configuration for StateService containing only required dependencies.
///
/// This configuration contains only the structured components that StateService actually needs,
/// maintaining logical groupings and avoiding over-destructuring.
#[derive(Debug, Clone)]
pub struct StateServiceConfig {
    /// Zebra validator with State backend configuration
    pub zebra: ZebradStateConfig,
    /// Service-level configuration (timeouts, channels)
    pub service: ServiceConfig,
    /// Storage configuration (cache, database)
    pub storage: StorageConfig,
    /// Network type for consensus calculations
    pub network: Network,
    /// Debug and testing configuration
    pub debug: DebugConfig,
}

impl From<(ZebradStateConfig, ZainodServiceConfig)> for StateServiceConfig {
    fn from((zebra, daemon): (ZebradStateConfig, ZainodServiceConfig)) -> Self {
        Self {
            zebra,
            service: daemon.service,
            storage: daemon.storage,
            network: daemon.network,
            debug: daemon.debug,
        }
    }
}

impl From<StateServiceConfig> for BlockCacheConfig {
    fn from(config: StateServiceConfig) -> Self {
        BlockCacheConfig {
            cache: config.storage.cache,
            database: config.storage.database,
            network: config.network,
            no_sync: config.debug.no_sync,
            no_db: config.debug.no_db,
        }
    }
}
