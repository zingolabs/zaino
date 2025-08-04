//! Configuration types for Zaino-Fetch services.

use zaino_commons::config::{BlockCacheConfig, ServiceConfig, ValidatorConfig};

/// Holds config data for [crate::FetchService].
#[derive(Debug, Clone)]
pub struct FetchServiceConfig {
    /// Validator connection and authentication configuration.
    pub validator: ValidatorConfig,
    /// Service-level configuration.
    pub service: ServiceConfig,
    /// Block cache configuration.
    pub block_cache: BlockCacheConfig,
}

impl FetchServiceConfig {
    /// Returns a new instance of [`FetchServiceConfig`].
    pub fn new(
        validator: ValidatorConfig,
        service: ServiceConfig,
        block_cache: BlockCacheConfig,
    ) -> Self {
        FetchServiceConfig {
            validator,
            service,
            block_cache,
        }
    }
}

impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(config: FetchServiceConfig) -> Self {
        config.block_cache
    }
}
