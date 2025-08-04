//! Common configuration types shared across Zaino crates.

use std::path::PathBuf;

/// Holds validator connection and authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ValidatorConfig {
    /// State service configuration
    pub config: ZainoStateConfig,
    /// Validator JsonRPC address.
    pub rpc_address: std::net::SocketAddr,
    /// Validator gRPC address.
    pub indexer_rpc_address: std::net::SocketAddr,
    /// Validator RPC cookie authentication
    pub cookie: CookieAuth,
    /// Validator JsonRPC user.
    pub rpc_user: String,
    /// Validator JsonRPC password.
    pub rpc_password: String,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            config: ZainoStateConfig::default(),
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            indexer_rpc_address: "127.0.0.1:8983".parse().expect("Valid socket address"),
            cookie: CookieAuth::Disabled,
            rpc_user: "xxxxxx".to_owned(),
            rpc_password: "xxxxxx".to_owned(),
        }
    }
}

/// Cookie-based authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CookieAuth {
    /// No cookie authentication
    Disabled,
    /// Cookie authentication enabled
    Enabled {
        /// Path to the cookie file
        path: PathBuf,
    },
}

impl Default for CookieAuth {
    fn default() -> Self {
        CookieAuth::Disabled
    }
}

/// Holds service-level configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
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
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
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
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
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

/// Holds config data for `[ChainIndex]`.
/// TODO: Rename when ChainIndex update is complete.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BlockCacheConfig {
    /// Cache configuration for DashMaps.
    pub cache: CacheConfig,
    /// Database configuration.
    pub database: DatabaseConfig,
    // todo! this porbably belongs in ValidatorConfig ... ?
    /// Network type.
    pub network: Network,
    /// Stops zaino waiting on server sync.
    /// Used for testing.
    pub no_sync: bool,
    /// Disables FinalisedState.
    /// Used for testing.
    pub no_db: bool,
}

/// Network type for Zaino configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Mainnet network
    Mainnet,
    /// Testnet network
    Testnet,
    /// Regtest network (for local testing)
    Regtest,
}

impl Network {
    /// Convert to Zebra's network type for internal use.
    pub fn to_zebra_network(&self) -> zebra_chain::parameters::Network {
        self.into()
    }
}

impl Into<zebra_chain::parameters::Network> for Network {
    fn into(self) -> zebra_chain::parameters::Network {
        match self {
            Network::Regtest => zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    nu6_1: None,
                    nu7: None,
                },
            ),
            Network::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
            Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
        }
    }
}

impl Into<zebra_chain::parameters::Network> for &Network {
    fn into(self) -> zebra_chain::parameters::Network {
        (*self).into()
    }
}

impl Default for Network {
    fn default() -> Self {
        Network::Testnet
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
/// Type of backend to be used.
pub enum BackendType {
    /// Uses ReadStateService (Zebra)
    State,
    /// Uses JsonRPC client (Zcashd. Zainod)
    Fetch,
}

/// Zaino's wrapper for zebra_state configuration.
/// 
/// This provides a clean public API while maintaining compatibility
/// with Zebra's internal state configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZainoStateConfig {
    /// Path to the directory for cached blockchain state
    pub cache_dir: std::path::PathBuf,
    /// If true, the state is stored in memory only and not persisted
    pub ephemeral: bool,
    /// If true, delete old database files on startup
    pub delete_old_database: bool,
    /// Optional height to stop processing blocks (for debugging)
    pub debug_stop_at_height: Option<u32>,
    /// Optional interval for validity checks (for debugging), e.g. "30s", "5min"
    #[serde(with = "humantime_serde")]
    pub debug_validity_check_interval: Option<std::time::Duration>,
}

impl Default for ZainoStateConfig {
    fn default() -> Self {
        Self {
            cache_dir: std::path::PathBuf::from("./zaino_state_cache"),
            ephemeral: false,
            delete_old_database: false,
            debug_stop_at_height: None,
            debug_validity_check_interval: None,
        }
    }
}

impl From<ZainoStateConfig> for zebra_state::Config {
    fn from(config: ZainoStateConfig) -> Self {
        zebra_state::Config {
            cache_dir: config.cache_dir,
            ephemeral: config.ephemeral,
            delete_old_database: config.delete_old_database,
            debug_stop_at_height: config.debug_stop_at_height,
            debug_validity_check_interval: config.debug_validity_check_interval,
        }
    }
}

impl From<zebra_state::Config> for ZainoStateConfig {
    fn from(config: zebra_state::Config) -> Self {
        Self {
            cache_dir: config.cache_dir,
            ephemeral: config.ephemeral,
            delete_old_database: config.delete_old_database,
            debug_stop_at_height: config.debug_stop_at_height,
            debug_validity_check_interval: config.debug_validity_check_interval,
        }
    }
}
