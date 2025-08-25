//! Configuration types for Zaino-Fetch services.

use zaino_commons::config::{BlockCacheConfig, DebugConfig, JsonRpcValidatorConfig, Network, ServiceConfig, StorageConfig, ZainodServiceConfig};

/// Minimal configuration for FetchService containing only required dependencies.
///
/// This configuration contains only the structured components that FetchService actually needs,
/// maintaining logical groupings and avoiding over-destructuring.
#[derive(Debug, Clone)]
pub struct FetchServiceConfig {
    /// JSON-RPC validator configuration - the validator for this service
    pub validator: JsonRpcValidatorConfig,
    /// Service-level configuration (timeouts, channels)
    pub service: ServiceConfig,
    /// Storage configuration (cache, database)
    pub storage: StorageConfig,
    /// Network type for consensus calculations
    pub network: Network,
    /// Debug and testing configuration
    pub debug: DebugConfig,
}

impl From<(JsonRpcValidatorConfig, ZainodServiceConfig)> for FetchServiceConfig {
    fn from((validator, daemon): (JsonRpcValidatorConfig, ZainodServiceConfig)) -> Self {
        Self {
            validator,
            service: daemon.service,
            storage: daemon.storage,
            network: daemon.network,
            debug: daemon.debug,
        }
    }
}

impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(config: FetchServiceConfig) -> Self {
        BlockCacheConfig {
            cache: config.storage.cache,
            database: config.storage.database,
            network: config.network,
            no_sync: config.debug.no_sync,
            no_db: config.debug.no_db,
        }
    }
}
