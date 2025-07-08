//! Zaino config.

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};

// Added for Serde deserialization helpers
use serde::{
    de::{self, Deserializer},
    Deserialize, Serialize,
};
#[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
use tracing::warn;
use tracing::{error, info};
use zaino_state::{BackendConfig, FetchServiceConfig, StateServiceConfig};

use crate::error::IndexerError;

/// Custom deserialization function for `SocketAddr` from a String.
/// Used by Serde's `deserialize_with`.
fn deserialize_socketaddr_from_string<'de, D>(deserializer: D) -> Result<SocketAddr, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    fetch_socket_addr_from_hostname(&s)
        .map_err(|e| de::Error::custom(format!("Invalid socket address string '{s}': {e}")))
}

/// Custom deserialization function for `BackendType` from a String.
/// Used by Serde's `deserialize_with`.
fn deserialize_backendtype_from_string<'de, D>(
    deserializer: D,
) -> Result<zaino_state::BackendType, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "state" => Ok(zaino_state::BackendType::State),
        "fetch" => Ok(zaino_state::BackendType::Fetch),
        _ => Err(de::Error::custom(format!(
            "Invalid backend type '{s}', valid options are 'state' or 'fetch'"
        ))),
    }
}

/// Config information required for Zaino.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IndexerConfig {
    /// Type of backend to be used.
    #[serde(deserialize_with = "deserialize_backendtype_from_string")]
    #[serde(serialize_with = "serialize_backendtype_to_string")]
    pub backend: zaino_state::BackendType,
    /// Enable JsonRPC server.
    pub enable_json_server: bool,
    /// Server bind addr.
    #[serde(deserialize_with = "deserialize_socketaddr_from_string")]
    pub json_rpc_listen_address: SocketAddr,
    /// Enable cookie-based authentication.
    pub enable_cookie_auth: bool,
    /// Directory to store authentication cookie file.
    pub cookie_dir: Option<PathBuf>,
    /// gRPC server bind addr.
    #[serde(deserialize_with = "deserialize_socketaddr_from_string")]
    pub grpc_listen_address: SocketAddr,
    /// Enables TLS.
    pub grpc_tls: bool,
    /// Path to the TLS certificate file.
    pub tls_cert_path: Option<String>,
    /// Path to the TLS private key file.
    pub tls_key_path: Option<String>,
    /// Full node / validator listen port.
    #[serde(deserialize_with = "deserialize_socketaddr_from_string")]
    pub validator_listen_address: SocketAddr,
    /// Enable validator rpc cookie authentification.
    pub validator_cookie_auth: bool,
    /// Path to the validator cookie file.
    pub validator_cookie_path: Option<String>,
    /// Full node / validator Username.
    pub validator_user: Option<String>,
    /// full node / validator Password.
    pub validator_password: Option<String>,
    /// Capacity of the Dashmaps used for the Mempool.
    /// Also use by the BlockCache::NonFinalisedState when using the FetchService.
    pub map_capacity: Option<usize>,
    /// Number of shard used in the DashMap used for the Mempool.
    /// Also use by the BlockCache::NonFinalisedState when using the FetchService.
    ///
    /// shard_amount should greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    pub map_shard_amount: Option<usize>,
    /// Block Cache database file path.
    ///
    /// ZainoDB location.
    pub zaino_db_path: PathBuf,
    /// Block Cache database file path.
    ///
    /// ZebraDB location.
    pub zebra_db_path: PathBuf,
    /// Block Cache database maximum size in gb.
    ///
    /// Only used by the FetchService.
    pub db_size: Option<usize>,
    /// Network chain type (Mainnet, Testnet, Regtest).
    pub network: String,
    /// Disables internal sync and stops zaino waiting on server sync.
    /// Used for testing.
    pub no_sync: bool,
    /// Disables FinalisedState.
    /// Used for testing.
    pub no_db: bool,
    /// When enabled Zaino syncs it DB in the background, fetching data from the validator.
    ///
    /// NOTE: Unimplemented.
    pub slow_sync: bool,
}

impl IndexerConfig {
    /// Performs checks on config data.
    pub(crate) fn check_config(&self) -> Result<(), IndexerError> {
        // Check network type.
        if (self.network != "Regtest") && (self.network != "Testnet") && (self.network != "Mainnet")
        {
            return Err(IndexerError::ConfigError(
                "Incorrect network name given, must be one of (Mainnet, Testnet, Regtest)."
                    .to_string(),
            ));
        }

        // Check TLS settings.
        if self.grpc_tls {
            if let Some(ref cert_path) = self.tls_cert_path {
                if !std::path::Path::new(cert_path).exists() {
                    return Err(IndexerError::ConfigError(format!(
                        "TLS is enabled, but certificate path '{cert_path}' does not exist."
                    )));
                }
            } else {
                return Err(IndexerError::ConfigError(
                    "TLS is enabled, but no certificate path is provided.".to_string(),
                ));
            }

            if let Some(ref key_path) = self.tls_key_path {
                if !std::path::Path::new(key_path).exists() {
                    return Err(IndexerError::ConfigError(format!(
                        "TLS is enabled, but key path '{key_path}' does not exist."
                    )));
                }
            } else {
                return Err(IndexerError::ConfigError(
                    "TLS is enabled, but no key path is provided.".to_string(),
                ));
            }
        }

        // Check validator cookie authentication settings
        if self.validator_cookie_auth {
            if let Some(ref cookie_path) = self.validator_cookie_path {
                if !std::path::Path::new(cookie_path).exists() {
                    return Err(IndexerError::ConfigError(
                        format!("Validator cookie authentication is enabled, but cookie path '{cookie_path}' does not exist."),
                    ));
                }
            } else {
                return Err(IndexerError::ConfigError(
                    "Validator cookie authentication is enabled, but no cookie path is provided."
                        .to_string(),
                ));
            }
        }

        #[cfg(not(feature = "disable_tls_unencrypted_traffic_mode"))]
        let grpc_addr = fetch_socket_addr_from_hostname(&self.grpc_listen_address.to_string())?;
        #[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
        let _ = fetch_socket_addr_from_hostname(&self.grpc_listen_address.to_string())?;

        let validator_addr =
            fetch_socket_addr_from_hostname(&self.validator_listen_address.to_string())?;

        // Ensure validator listen address is private.
        if !is_private_listen_addr(&validator_addr) {
            return Err(IndexerError::ConfigError(
                "Zaino may only connect to Zebra with private IP addresses.".to_string(),
            ));
        }

        #[cfg(not(feature = "disable_tls_unencrypted_traffic_mode"))]
        {
            // Ensure TLS is used when connecting to external addresses.
            if !is_private_listen_addr(&grpc_addr) && !self.grpc_tls {
                return Err(IndexerError::ConfigError(
                    "TLS required when connecting to external addresses.".to_string(),
                ));
            }

            // Ensure validator rpc cookie authentication is used when connecting to non-loopback addresses.
            if !is_loopback_listen_addr(&validator_addr) && !self.validator_cookie_auth {
                return Err(IndexerError::ConfigError(
                "Validator listen address is not loopback, so cookie authentication must be enabled."
                    .to_string(),
            ));
            }
        }
        #[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
        {
            warn!("Zaino built using disable_tls_unencrypted_traffic_mode feature, proceed with caution.");
        }

        // Check gRPC and JsonRPC server are not listening on the same address.
        if self.json_rpc_listen_address == self.grpc_listen_address {
            return Err(IndexerError::ConfigError(
                "gRPC server and JsonRPC server must listen on different addresses.".to_string(),
            ));
        }

        Ok(())
    }

    /// Returns the network type currently being used by the server.
    pub fn get_network(&self) -> Result<zebra_chain::parameters::Network, IndexerError> {
        match self.network.as_str() {
            "Regtest" => Ok(zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
                    nu6_1: None,
                    nu7: None,
                },
            )),
            "Testnet" => Ok(zebra_chain::parameters::Network::new_default_testnet()),
            "Mainnet" => Ok(zebra_chain::parameters::Network::Mainnet),
            _ => Err(IndexerError::ConfigError(
                "Incorrect network name given.".to_string(),
            )),
        }
    }

    /// Finalizes the configuration after initial parsing, applying conditional defaults.
    fn finalize_config_logic(mut self) -> Self {
        if self.enable_cookie_auth {
            if self.cookie_dir.is_none() {
                self.cookie_dir = Some(default_ephemeral_cookie_path());
            }
        } else {
            // If auth is not enabled, cookie_dir should be None, regardless of what was in the config.
            self.cookie_dir = None;
        }
        self
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            backend: zaino_state::BackendType::Fetch,
            enable_json_server: false,
            json_rpc_listen_address: "127.0.0.1:8237".parse().unwrap(),
            enable_cookie_auth: false,
            cookie_dir: None,
            grpc_listen_address: "127.0.0.1:8137".parse().unwrap(),
            grpc_tls: false,
            tls_cert_path: None,
            tls_key_path: None,
            validator_listen_address: "127.0.0.1:18232".parse().unwrap(),
            validator_cookie_auth: false,
            validator_cookie_path: None,
            validator_user: Some("xxxxxx".to_string()),
            validator_password: Some("xxxxxx".to_string()),
            map_capacity: None,
            map_shard_amount: None,
            zaino_db_path: default_zaino_db_path(),
            zebra_db_path: default_zebra_db_path().unwrap(),
            db_size: None,
            network: "Testnet".to_string(),
            no_sync: false,
            no_db: false,
            slow_sync: false,
        }
    }
}

/// Returns the default path for Zaino's ephemeral authentication cookie.
pub fn default_ephemeral_cookie_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("zaino").join(".cookie")
    } else {
        PathBuf::from("/tmp").join("zaino").join(".cookie")
    }
}

/// Loads the default file path for zaino's local db.
pub fn default_zaino_db_path() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".cache").join("zaino"),
        Err(_) => PathBuf::from("/tmp").join("zaino").join(".cache"),
    }
}

/// Loads the default file path for zebras's local db.
pub fn default_zebra_db_path() -> Result<PathBuf, IndexerError> {
    match std::env::var("HOME") {
        Ok(home) => Ok(PathBuf::from(home).join(".cache").join("zebra")),
        Err(e) => Err(IndexerError::ConfigError(format!(
            "Unable to find home directory: {e}",
        ))),
    }
}

/// Resolves a hostname to a SocketAddr.
fn fetch_socket_addr_from_hostname(address: &str) -> Result<SocketAddr, IndexerError> {
    address.parse::<SocketAddr>().or_else(|_| {
        let addrs: Vec<_> = address
            .to_socket_addrs()
            .map_err(|e| IndexerError::ConfigError(format!("Invalid address '{address}': {e}")))?
            .collect();
        if let Some(ipv4_addr) = addrs.iter().find(|addr| addr.is_ipv4()) {
            Ok(*ipv4_addr)
        } else {
            addrs.into_iter().next().ok_or_else(|| {
                IndexerError::ConfigError(format!("Unable to resolve address '{address}'"))
            })
        }
    })
}

/// Validates that the configured `address` is either:
/// - An RFC1918 (private) IPv4 address, or
/// - An IPv6 Unique Local Address (ULA) (using `is_unique_local()`)
///
/// Returns `Ok(BindAddress)` if valid.
pub(crate) fn is_private_listen_addr(addr: &SocketAddr) -> bool {
    let ip = addr.ip();
    match ip {
        IpAddr::V4(ipv4) => ipv4.is_private() || ipv4.is_loopback(),
        IpAddr::V6(ipv6) => ipv6.is_unique_local() || ip.is_loopback(),
    }
}

/// Validates that the configured `address` is a loopback address.
///
/// Returns `Ok(BindAddress)` if valid.
#[cfg_attr(feature = "disable_tls_unencrypted_traffic_mode", allow(dead_code))]
pub(crate) fn is_loopback_listen_addr(addr: &SocketAddr) -> bool {
    let ip = addr.ip();
    match ip {
        IpAddr::V4(ipv4) => ipv4.is_loopback(),
        IpAddr::V6(ipv6) => ipv6.is_loopback(),
    }
}

/// Attempts to load config data from a TOML file at the specified path.
///
/// If the file cannot be read, or if its contents cannot be parsed into `IndexerConfig`,
/// a warning is logged, and a default configuration is returned.
/// The loaded or default configuration undergoes further checks and finalization.
pub fn load_config(file_path: &PathBuf) -> Result<IndexerConfig, IndexerError> {
    // Configuration sources are layered: Env > TOML > Defaults.
    let figment = Figment::new()
        // 1. Base defaults from `IndexerConfig::default()`.
        .merge(Serialized::defaults(IndexerConfig::default()))
        // 2. Override with values from the TOML configuration file.
        .merge(Toml::file(file_path))
        // 3. Override with values from environment variables prefixed with "ZAINO_".
        .merge(figment::providers::Env::prefixed("ZAINO_"));

    match figment.extract::<IndexerConfig>() {
        Ok(parsed_config) => {
            let finalized_config = parsed_config.finalize_config_logic();
            finalized_config.check_config()?;
            info!(
                "Successfully loaded and validated config. Base TOML file checked: '{}'",
                file_path.display()
            );
            Ok(finalized_config)
        }
        Err(figment_error) => {
            error!("Failed to extract configuration: {}", figment_error);
            Err(IndexerError::ConfigError(format!(
                "Configuration loading failed for TOML file '{}' (or environment variables). Details: {}",
                file_path.display(), figment_error
            )))
        }
    }
}

impl TryFrom<IndexerConfig> for BackendConfig {
    type Error = IndexerError;

    fn try_from(cfg: IndexerConfig) -> Result<Self, Self::Error> {
        let network = cfg.get_network()?;

        match cfg.backend {
            zaino_state::BackendType::State => Ok(BackendConfig::State(StateServiceConfig {
                validator_config: zebra_state::Config {
                    cache_dir: cfg.zebra_db_path.clone(),
                    ephemeral: false,
                    delete_old_database: true,
                    debug_stop_at_height: None,
                    debug_validity_check_interval: None,
                },
                validator_rpc_address: cfg.validator_listen_address,
                validator_cookie_auth: cfg.validator_cookie_auth,
                validator_cookie_path: cfg.validator_cookie_path,
                validator_rpc_user: cfg.validator_user.unwrap_or_else(|| "xxxxxx".to_string()),
                validator_rpc_password: cfg
                    .validator_password
                    .unwrap_or_else(|| "xxxxxx".to_string()),
                service_timeout: 30,
                service_channel_size: 32,
                map_capacity: cfg.map_capacity,
                map_shard_amount: cfg.map_shard_amount,
                db_path: cfg.zaino_db_path,
                db_size: cfg.db_size,
                network,
                no_sync: cfg.no_sync,
                no_db: cfg.no_db,
            })),

            zaino_state::BackendType::Fetch => Ok(BackendConfig::Fetch(FetchServiceConfig {
                validator_rpc_address: cfg.validator_listen_address,
                validator_cookie_auth: cfg.validator_cookie_auth,
                validator_cookie_path: cfg.validator_cookie_path,
                validator_rpc_user: cfg.validator_user.unwrap_or_else(|| "xxxxxx".to_string()),
                validator_rpc_password: cfg
                    .validator_password
                    .unwrap_or_else(|| "xxxxxx".to_string()),
                service_timeout: 30,
                service_channel_size: 32,
                map_capacity: cfg.map_capacity,
                map_shard_amount: cfg.map_shard_amount,
                db_path: cfg.zaino_db_path,
                db_size: cfg.db_size,
                network,
                no_sync: cfg.no_sync,
                no_db: cfg.no_db,
            })),
        }
    }
}

/// Custom serializer for BackendType
fn serialize_backendtype_to_string<S>(
    backend_type: &zaino_state::BackendType,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(match backend_type {
        zaino_state::BackendType::State => "state",
        zaino_state::BackendType::Fetch => "fetch",
    })
}
