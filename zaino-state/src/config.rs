//! Holds config data for Zaino-State services.

pub use zaino_commons::config::{BlockCacheConfig, ZainodServiceConfig, ZebradStateConfig};

/// Type-safe configuration for StateService.
/// 
/// This ensures that only valid Zebra + State backend configurations
/// can be passed to the State service, preventing runtime errors.
#[derive(Debug, Clone)]
pub struct StateServiceConfig {
    /// Zebra validator with State backend configuration (type-safe)
    pub zebrad: ZebradStateConfig,
    /// Zaino daemon service configuration
    pub zainod: ZainodServiceConfig,
}


impl From<StateServiceConfig> for BlockCacheConfig {
    fn from(config: StateServiceConfig) -> Self {
        config.block_cache
    }
}
