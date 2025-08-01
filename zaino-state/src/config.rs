//! Holds config data for Zaino-State services.

use std::path::PathBuf;

/// Holds validator connection and authentication configuration.
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    /// Zebra [`zebra_state::ReadStateService`] config data
    pub config: zebra_state::Config,
    /// Validator JsonRPC address.
    pub rpc_address: std::net::SocketAddr,
    /// Validator gRPC address.
    pub indexer_rpc_address: std::net::SocketAddr,
    /// Enable validator rpc cookie authentification.
    pub cookie_auth: bool,
    /// Path to the validator cookie file.
    pub cookie_path: Option<String>,
    /// Validator JsonRPC user.
    pub rpc_user: String,
    /// Validator JsonRPC password.
    pub rpc_password: String,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            config: zebra_state::Config::default(),
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            indexer_rpc_address: "127.0.0.1:8983".parse().expect("Valid socket address"),
            cookie_auth: false,
            cookie_path: None,
            rpc_user: "xxxxxx".to_owned(),
            rpc_password: "xxxxxx".to_owned(),
        }
    }
}

/// Holds service-level configuration.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// StateService RPC timeout
    pub timeout: u32,
    /// StateService RPC max channel size.
    pub channel_size: u32,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            timeout: 30,
            channel_size: 32,
        }
    }
}

/// Holds cache configuration for DashMaps.
#[derive(Debug, Clone, Default)]
pub struct CacheConfig {
    /// Capacity of the Dashmaps used for the Mempool and BlockCache NonFinalisedState.
    pub capacity: Option<usize>,
    /// Number of shard used in the DashMap used for the Mempool and BlockCache NonFinalisedState.
    ///
    /// shard_amount should greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    pub shard_amount: Option<usize>,
}

/// Holds database configuration.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Block Cache database file path.
    pub path: PathBuf,
    /// Block Cache database maximum size in gb.
    pub size: Option<usize>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./zaino_cache"),
            size: None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
/// Type of backend to be used.
pub enum BackendType {
    /// Uses ReadStateService (Zebrad)
    State,
    /// Uses JsonRPC client (Zcashd. Zainod)
    Fetch,
}

#[derive(Debug, Clone)]
/// Unified backend configuration enum.
pub enum BackendConfig {
    /// StateService config.
    State(StateServiceConfig),
    /// Fetchservice config.
    Fetch(FetchServiceConfig),
}

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

/// Holds config data for `[ChainIndex]`.
/// TODO: Rename when ChainIndex update is complete.
#[derive(Debug, Clone)]
pub struct BlockCacheConfig {
    /// Cache configuration for DashMaps.
    pub cache: CacheConfig,
    /// Database configuration.
    pub database: DatabaseConfig,
    /// Network type.
    pub network: zebra_chain::parameters::Network,
    /// Stops zaino waiting on server sync.
    /// Used for testing.
    pub no_sync: bool,
    /// Disables FinalisedState.
    /// Used for testing.
    pub no_db: bool,
}

impl BlockCacheConfig {
    /// Returns a new instance of [`BlockCacheConfig`].
    #[allow(dead_code)]
    pub fn new(
        cache: CacheConfig,
        database: DatabaseConfig,
        network: zebra_chain::parameters::Network,
        no_sync: bool,
        no_db: bool,
    ) -> Self {
        BlockCacheConfig {
            cache,
            database,
            network,
            no_sync,
            no_db,
        }
    }
}

impl From<StateServiceConfig> for BlockCacheConfig {
    fn from(config: StateServiceConfig) -> Self {
        config.block_cache
    }
}

impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(config: FetchServiceConfig) -> Self {
        config.block_cache
    }
}
