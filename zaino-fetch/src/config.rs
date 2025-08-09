//! Configuration types for Zaino-Fetch services.

use zaino_commons::config::{BlockCacheConfig, ValidatorFetchConfig, ZainodServiceConfig};

/// Type-safe configuration for FetchService.
/// 
/// This ensures that only valid validator + Fetch backend configurations
/// can be passed to the Fetch service, preventing runtime errors.
#[derive(Debug, Clone)]
pub struct FetchServiceConfig {
    /// Validator with Fetch backend configuration (type-safe)
    pub validator: ValidatorFetchConfig,
    /// Zaino daemon service configuration
    pub zainod: ZainodServiceConfig,
}


impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(config: FetchServiceConfig) -> Self {
        BlockCacheConfig {
            cache: config.zainod.cache,
            database: config.zainod.database,
            network: config.zainod.network,
            no_sync: config.zainod.debug.no_sync,
            no_db: config.zainod.debug.no_db,
        }
    }
}
