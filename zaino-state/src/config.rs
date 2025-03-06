//! Holds config data for Zaino-State services.

use std::path::PathBuf;

/// Holds config data for [`StateService`].
#[derive(Debug, Clone)]
pub struct StateServiceConfig {
    /// Zebra [`ReadStateService`] config data
    pub validator_config: zebra_state::Config,
    /// Validator JsonRPC address.
    pub validator_rpc_address: std::net::SocketAddr,
    /// Enable validator rpc cookie authentification.
    pub validator_cookie_auth: bool,
    /// Path to the validator cookie file.
    pub validator_cookie_path: Option<String>,
    /// Validator JsonRPC user.
    pub validator_rpc_user: String,
    /// Validator JsonRPC password.
    pub validator_rpc_password: String,
    /// StateService RPC timeout
    pub service_timeout: u32,
    /// StateService RPC max channel size.
    pub service_channel_size: u32,
    /// StateService network type.
    pub network: zebra_chain::parameters::Network,
}

impl StateServiceConfig {
    /// Returns a new instance of [`StateServiceConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_config: zebra_state::Config,
        validator_rpc_address: std::net::SocketAddr,
        validator_cookie_auth: bool,
        validator_cookie_path: Option<String>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service_timeout: Option<u32>,
        service_channel_size: Option<u32>,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        StateServiceConfig {
            validator_config,
            validator_rpc_address,
            validator_cookie_auth,
            validator_cookie_path,
            validator_rpc_user: validator_rpc_user.unwrap_or("xxxxxx".to_string()),
            validator_rpc_password: validator_rpc_password.unwrap_or("xxxxxx".to_string()),
            service_timeout: service_timeout.unwrap_or(30),
            service_channel_size: service_channel_size.unwrap_or(32),
            network,
        }
    }
}

/// Holds config data for [`FetchService`].
#[derive(Debug, Clone)]
pub struct FetchServiceConfig {
    /// Validator JsonRPC address.
    pub validator_rpc_address: std::net::SocketAddr,
    /// Enable validator rpc cookie authentification.
    pub validator_cookie_auth: bool,
    /// Path to the validator cookie file.
    pub validator_cookie_path: Option<String>,
    /// Validator JsonRPC user.
    pub validator_rpc_user: String,
    /// Validator JsonRPC password.
    pub validator_rpc_password: String,
    /// StateService RPC timeout
    pub service_timeout: u32,
    /// StateService RPC max channel size.
    pub service_channel_size: u32,
    /// Capacity of the Dashmaps used for the Mempool and BlockCache NonFinalisedState.
    pub map_capacity: Option<usize>,
    /// Number of shard used in the DashMap used for the Mempool and BlockCache NonFinalisedState.
    ///
    /// shard_amount should greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    pub map_shard_amount: Option<usize>,
    /// Block Cache database file path.
    pub db_path: PathBuf,
    /// Block Cache database maximum size in gb.
    pub db_size: Option<usize>,
    /// Network type.
    pub network: zebra_chain::parameters::Network,
    /// Disables internal sync and stops zaino waiting on server sync.
    /// Used for testing.
    pub no_sync: bool,
    /// Disables FinalisedState.
    /// Used for testing.
    pub no_db: bool,
}

impl FetchServiceConfig {
    /// Returns a new instance of [`FetchServiceConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_rpc_address: std::net::SocketAddr,
        validator_cookie_auth: bool,
        validator_cookie_path: Option<String>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service_timeout: Option<u32>,
        service_channel_size: Option<u32>,
        map_capacity: Option<usize>,
        map_shard_amount: Option<usize>,
        db_path: PathBuf,
        db_size: Option<usize>,
        network: zebra_chain::parameters::Network,
        no_sync: bool,
        no_db: bool,
    ) -> Self {
        FetchServiceConfig {
            validator_rpc_address,
            validator_cookie_auth,
            validator_cookie_path,
            validator_rpc_user: validator_rpc_user.unwrap_or("xxxxxx".to_string()),
            validator_rpc_password: validator_rpc_password.unwrap_or("xxxxxx".to_string()),
            // NOTE: This timeout is currently long to ease development but should be reduced before production.
            service_timeout: service_timeout.unwrap_or(60),
            service_channel_size: service_channel_size.unwrap_or(32),
            map_capacity,
            map_shard_amount,
            db_path,
            db_size,
            network,
            no_sync,
            no_db,
        }
    }
}

/// Holds config data for [`FetchService`].
#[derive(Debug, Clone)]
pub struct BlockCacheConfig {
    /// Capacity of the Dashmap.
    ///
    /// NOTE: map_capacity and shard map must both be set for either to be used.
    pub map_capacity: Option<usize>,
    /// Number of shard used in the DashMap.
    ///
    /// shard_amount should greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    ///
    /// NOTE: map_capacity and shard map must both be set for either to be used.
    pub map_shard_amount: Option<usize>,
    /// Block Cache database file path.
    pub db_path: PathBuf,
    /// Block Cache database maximum size in gb.
    pub db_size: Option<usize>,
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
    /// Returns a new instance of [`FetchServiceConfig`].
    pub fn new(
        map_capacity: Option<usize>,
        map_shard_amount: Option<usize>,
        db_path: PathBuf,
        db_size: Option<usize>,
        network: zebra_chain::parameters::Network,
        no_sync: bool,
        no_db: bool,
    ) -> Self {
        BlockCacheConfig {
            map_capacity,
            map_shard_amount,
            db_path,
            db_size,
            network,
            no_sync,
            no_db,
        }
    }
}

impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(value: FetchServiceConfig) -> Self {
        Self {
            map_capacity: value.map_capacity,
            map_shard_amount: value.map_shard_amount,
            db_path: value.db_path,
            db_size: value.db_size,
            network: value.network,
            no_sync: value.no_sync,
            no_db: value.no_db,
        }
    }
}
