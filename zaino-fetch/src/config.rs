//! Configuration types for Zaino-Fetch services.

use zaino_commons::config::{BlockCacheConfig, JsonRpcValidatorConfig, ZainodServiceConfig};

/// Type-safe configuration for FetchService.
///
/// This ensures that only valid validator + Fetch backend configurations
/// can be passed to the Fetch service, preventing runtime errors.
#[derive(Debug, Clone)]
pub struct FetchServiceConfig {
    /// JSON-RPC validator configuration - the validator for this service
    pub validator: JsonRpcValidatorConfig,
    /// Zaino daemon service configuration
    pub daemon: ZainodServiceConfig,
}

impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(config: FetchServiceConfig) -> Self {
        BlockCacheConfig {
            cache: config.daemon.storage.cache,
            database: config.daemon.storage.database,
            network: config.daemon.network,
            no_sync: config.daemon.debug.no_sync,
            no_db: config.daemon.debug.no_db,
        }
    }
}
