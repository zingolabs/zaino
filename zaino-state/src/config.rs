//! Holds config data for Zaino-State services.

use std::path::PathBuf;
use zaino_common::{Network, ServiceConfig, StorageConfig};

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
/// Type of backend to be used.
pub enum BackendType {
    /// Uses ReadStateService (Zebrad)
    State,
    /// Uses JsonRPC client (Zcashd. Zainod)
    Fetch,
}

/// Unified backend configuration enum.
#[derive(Debug, Clone)]
#[allow(deprecated)]
pub enum BackendConfig {
    /// StateService config.
    State(StateServiceConfig),
    /// Fetchservice config.
    Fetch(FetchServiceConfig),
}

/// Holds config data for [crate::StateService].
#[derive(Debug, Clone)]
#[deprecated]
pub struct StateServiceConfig {
    /// Zebra [`zebra_state::ReadStateService`] config data
    pub validator_state_config: zebra_state::Config,
    /// Validator JsonRPC address.
    pub validator_rpc_address: std::net::SocketAddr,
    /// Validator gRPC address.
    pub validator_grpc_address: std::net::SocketAddr,
    /// Validator cookie auth.
    pub validator_cookie_auth: bool,
    /// Enable validator rpc cookie authentification with Some: Path to the validator cookie file.
    pub validator_cookie_path: Option<PathBuf>,
    /// Validator JsonRPC user.
    pub validator_rpc_user: String,
    /// Validator JsonRPC password.
    pub validator_rpc_password: String,
    /// Service-level configuration (timeout, channel size)
    pub service: ServiceConfig,
    /// Storage configuration (cache and database)
    pub storage: StorageConfig,
    /// Network type.
    pub network: Network,
}

#[allow(deprecated)]
impl StateServiceConfig {
    /// Returns a new instance of [`StateServiceConfig`].
    #[allow(clippy::too_many_arguments)]
    // TODO: replace with struct-literal init only?
    pub fn new(
        validator_state_config: zebra_state::Config,
        validator_rpc_address: std::net::SocketAddr,
        validator_grpc_address: std::net::SocketAddr,
        validator_cookie_auth: bool,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        network: Network,
    ) -> Self {
        StateServiceConfig {
            validator_state_config,
            validator_rpc_address,
            validator_grpc_address,
            validator_cookie_auth,
            validator_cookie_path,
            validator_rpc_user: validator_rpc_user.unwrap_or("xxxxxx".to_string()),
            validator_rpc_password: validator_rpc_password.unwrap_or("xxxxxx".to_string()),
            service,
            storage,
            network,
        }
    }
}

/// Holds config data for [crate::FetchService].
#[derive(Debug, Clone)]
#[deprecated]
pub struct FetchServiceConfig {
    /// Validator JsonRPC address.
    pub validator_rpc_address: std::net::SocketAddr,
    /// Enable validator rpc cookie authentification with Some: path to the validator cookie file.
    pub validator_cookie_path: Option<PathBuf>,
    /// Validator JsonRPC user.
    pub validator_rpc_user: String,
    /// Validator JsonRPC password.
    pub validator_rpc_password: String,
    /// Service-level configuration (timeout, channel size)
    pub service: ServiceConfig,
    /// Storage configuration (cache and database)
    pub storage: StorageConfig,
    /// Network type.
    pub network: Network,
}

#[allow(deprecated)]
impl FetchServiceConfig {
    /// Returns a new instance of [`FetchServiceConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_rpc_address: std::net::SocketAddr,
        validator_cookie_path: Option<PathBuf>,
        validator_rpc_user: Option<String>,
        validator_rpc_password: Option<String>,
        service: ServiceConfig,
        storage: StorageConfig,
        network: Network,
    ) -> Self {
        FetchServiceConfig {
            validator_rpc_address,
            validator_cookie_path,
            validator_rpc_user: validator_rpc_user.unwrap_or("xxxxxx".to_string()),
            validator_rpc_password: validator_rpc_password.unwrap_or("xxxxxx".to_string()),
            service,
            storage,
            network,
        }
    }
}

/// Holds config data for `[ZainoDb]`.
/// TODO: Rename  to *ZainoDbConfig* when ChainIndex update is complete **and** remove legacy fields.
#[derive(Debug, Clone)]
pub struct BlockCacheConfig {
    /// Storage configuration (cache and database)
    pub storage: StorageConfig,
    /// Database version selected to be run.
    pub db_version: u32,
    /// Network type.
    pub network: Network,
}

impl BlockCacheConfig {
    /// Returns a new instance of [`BlockCacheConfig`].
    #[allow(dead_code)]
    pub fn new(storage: StorageConfig, db_version: u32, network: Network, _no_sync: bool) -> Self {
        BlockCacheConfig {
            storage,
            db_version,
            network,
        }
    }
}

#[allow(deprecated)]
impl From<StateServiceConfig> for BlockCacheConfig {
    fn from(value: StateServiceConfig) -> Self {
        Self {
            storage: value.storage,
            // TODO: update zaino configs to include db version.
            db_version: 1,
            network: value.network,
        }
    }
}

#[allow(deprecated)]
impl From<FetchServiceConfig> for BlockCacheConfig {
    fn from(value: FetchServiceConfig) -> Self {
        Self {
            storage: value.storage,
            // TODO: update zaino configs to include db version.
            db_version: 1,
            network: value.network,
        }
    }
}
