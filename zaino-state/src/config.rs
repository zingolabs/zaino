//! Holds config data for Zaino-State services.

pub use zaino_commons::config::{BlockCacheConfig, ServiceConfig, ValidatorConfig};

/// Holds config data for [crate::StateService].
#[derive(Debug, Clone)]
pub struct StateServiceConfig {
    /// Validator connection and authentication configuration.
    pub validator: ValidatorConfig,
    /// Service-level configuration.
    pub service: ServiceConfig,
    /// Block cache configuration.
    pub block_cache: BlockCacheConfig,
}

impl StateServiceConfig {
    /// Returns a new instance of [`StateServiceConfig`].
    pub fn new(
        validator: ValidatorConfig,
        service: ServiceConfig,
        block_cache: BlockCacheConfig,
    ) -> Self {
        StateServiceConfig {
            validator,
            service,
            block_cache,
        }
    }
}

impl From<StateServiceConfig> for BlockCacheConfig {
    fn from(config: StateServiceConfig) -> Self {
        config.block_cache
    }
}
