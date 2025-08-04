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
use zaino_commons::config::{
    BackendType, BlockCacheConfig, CacheConfig, CookieAuth, DatabaseConfig, Network, ServiceConfig,
    ValidatorConfig,
};
use zaino_fetch::config::FetchServiceConfig;
use zaino_state::StateServiceConfig;

use crate::error::IndexerError;

/// Unified backend configuration enum.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    /// StateService config.
    State(StateServiceConfig),
    /// Fetchservice config.
    Fetch(FetchServiceConfig),
}

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
fn deserialize_backendtype_from_string<'de, D>(deserializer: D) -> Result<BackendType, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "state" => Ok(BackendType::State),
        "fetch" => Ok(BackendType::Fetch),
        _ => Err(de::Error::custom(format!(
            "Invalid backend type '{s}', valid options are 'state' or 'fetch'"
        ))),
    }
}

/// Server configuration for Zaino's own servers (JSON-RPC and gRPC).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Enable JsonRPC server.
    pub enable_json_server: bool,
    /// JsonRPC server bind address.
    #[serde(deserialize_with = "deserialize_socketaddr_from_string")]
    pub json_rpc_listen_address: SocketAddr,
    /// Enable cookie-based authentication for zaino server.
    pub cookie: CookieAuth,
    /// gRPC server bind address.
    #[serde(deserialize_with = "deserialize_socketaddr_from_string")]
    pub grpc_listen_address: SocketAddr,
    /// Enables TLS for gRPC server.
    pub grpc_tls: bool,
    /// Path to the TLS certificate file.
    pub tls_cert_path: Option<String>,
    /// Path to the TLS private key file.
    pub tls_key_path: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enable_json_server: false,
            json_rpc_listen_address: "127.0.0.1:8237".parse().unwrap(),
            cookie: CookieAuth::Disabled,
            grpc_listen_address: "127.0.0.1:8137".parse().unwrap(),
            grpc_tls: false,
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

/// Storage configuration (cache and database settings).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Cache configuration.
    pub cache: CacheConfig,
    /// Zaino database configuration.
    pub zaino_database: DatabaseConfig,
    /// Zebra database configuration.
    pub zebra_database: DatabaseConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            cache: CacheConfig::default(),
            zaino_database: DatabaseConfig {
                path: default_zaino_db_path(),
                size: None,
            },
            zebra_database: DatabaseConfig {
                path: default_zebra_db_path().unwrap(),
                size: None,
            },
        }
    }
}

/// Debug and testing configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DebugConfig {
    /// Disables internal sync and stops zaino waiting on server sync.
    /// Used for testing.
    pub no_sync: bool,
    /// Disables FinalisedState.
    /// Used for testing.
    pub no_db: bool,
    /// When enabled Zaino syncs it DB in the background, fetching data from the validator.
    /// NOTE: Unimplemented.
    pub slow_sync: bool,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            no_sync: false,
            no_db: false,
            slow_sync: false,
        }
    }
}

/// Config information required for Zaino.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IndexerConfig {
    /// Type of backend to be used.
    pub backend: BackendType,
    /// Network type (Mainnet, Testnet, Regtest).
    pub network: Network,
    /// Server configuration (zaino's own JSON-RPC and gRPC servers).
    pub server: ServerConfig,
    /// Validator connection and authentication configuration.
    pub validator: ValidatorConfig,
    /// Service-level configuration.
    pub service: ServiceConfig,
    /// Storage configuration (cache and database).
    pub storage: StorageConfig,
    /// Debug and testing configuration.
    pub debug: DebugConfig,
}

impl IndexerConfig {
    /// Performs checks on config data.
    pub(crate) fn check_config(&self) -> Result<(), IndexerError> {
        // Network validation is now handled by the Network enum, no string checking needed

        // Check TLS settings.
        if self.server.grpc_tls {
            if let Some(ref cert_path) = self.server.tls_cert_path {
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

            if let Some(ref key_path) = self.server.tls_key_path {
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
        if let CookieAuth::Enabled { ref path } = self.validator.cookie {
            if !path.exists() {
                return Err(IndexerError::ConfigError(
                    format!("Validator cookie authentication is enabled, but cookie path '{}' does not exist.", path.display()),
                ));
            }
        }

        #[cfg(not(feature = "disable_tls_unencrypted_traffic_mode"))]
        let grpc_addr = fetch_socket_addr_from_hostname(&self.server.grpc_listen_address.to_string())?;
        #[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
        let _ = fetch_socket_addr_from_hostname(&self.server.grpc_listen_address.to_string())?;

        let validator_addr =
            fetch_socket_addr_from_hostname(&self.validator.rpc_address.to_string())?;

        // Ensure validator listen address is private.
        if !is_private_listen_addr(&validator_addr) {
            return Err(IndexerError::ConfigError(
                "Zaino may only connect to Zebra with private IP addresses.".to_string(),
            ));
        }

        #[cfg(not(feature = "disable_tls_unencrypted_traffic_mode"))]
        {
            // Ensure TLS is used when connecting to external addresses.
            if !is_private_listen_addr(&grpc_addr) && !self.server.grpc_tls {
                return Err(IndexerError::ConfigError(
                    "TLS required when connecting to external addresses.".to_string(),
                ));
            }

            // Ensure validator rpc cookie authentication is used when connecting to non-loopback addresses.
            if !is_loopback_listen_addr(&validator_addr) {
                if let CookieAuth::Disabled = self.validator.cookie {
                    return Err(IndexerError::ConfigError(
                        "Validator listen address is not loopback, so cookie authentication must be enabled."
                            .to_string(),
                    ));
                }
            }
        }
        #[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
        {
            warn!("Zaino built using disable_tls_unencrypted_traffic_mode feature, proceed with caution.");
        }

        // Check gRPC and JsonRPC server are not listening on the same address.
        if self.server.json_rpc_listen_address == self.server.grpc_listen_address {
            return Err(IndexerError::ConfigError(
                "gRPC server and JsonRPC server must listen on different addresses.".to_string(),
            ));
        }

        Ok(())
    }


    /// Finalizes the configuration after initial parsing, applying conditional defaults.
    fn finalize_config_logic(mut self) -> Self {
        // Ensure cookie path is set for enabled cookie auth
        if let CookieAuth::Enabled { ref path } = self.server.cookie {
            if path.as_os_str().is_empty() {
                self.server.cookie = CookieAuth::Enabled {
                    path: default_ephemeral_cookie_path(),
                };
            }
        }
        self
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            backend: BackendType::Fetch,
            network: Network::Testnet,
            server: ServerConfig::default(),
            validator: ValidatorConfig {
                config: zebra_state::Config::default(),
                rpc_address: "127.0.0.1:18232".parse().unwrap(),
                indexer_rpc_address: "127.0.0.1:18230".parse().unwrap(),
                cookie: CookieAuth::Disabled,
                rpc_user: "xxxxxx".to_string(),
                rpc_password: "xxxxxx".to_string(),
            },
            service: ServiceConfig::default(),
            storage: StorageConfig::default(),
            debug: DebugConfig::default(),
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
            BackendType::State => Ok(BackendConfig::State(StateServiceConfig {
                validator: ValidatorConfig {
                    config: zebra_state::Config {
                        cache_dir: cfg.zebra_db_path.clone(),
                        ephemeral: false,
                        delete_old_database: true,
                        debug_stop_at_height: None,
                        debug_validity_check_interval: None,
                    },
                    // todo! the whole IndexerConfig struct should also be refactored into sub-structs and all this can be avoided
                    cookie: {
                        if cfg.enable_cookie_auth {
                            CookieAuth::Enabled {
                                path: cfg
                                    .cookie_dir
                                    .expect("should provide cookie dir if enable_cookie_auth=true"),
                            }
                        } else {
                            CookieAuth::Disabled
                        }
                    },
                    rpc_address: cfg.validator_listen_address,
                    indexer_rpc_address: cfg.validator_grpc_listen_address,
                    rpc_user: cfg.validator_user.unwrap_or_else(|| "xxxxxx".to_string()),
                    rpc_password: cfg
                        .validator_password
                        .unwrap_or_else(|| "xxxxxx".to_string()),
                },
                service: ServiceConfig {
                    timeout: 30,
                    channel_size: 32,
                },
                block_cache: BlockCacheConfig {
                    cache: CacheConfig {
                        capacity: cfg.map_capacity,
                        shard_amount: cfg.map_shard_amount,
                    },
                    database: DatabaseConfig {
                        path: cfg.zaino_db_path,
                        size: cfg.db_size,
                    },
                    network,
                    no_sync: cfg.no_sync,
                    no_db: cfg.no_db,
                },
            })),

            BackendType::Fetch => Ok(BackendConfig::Fetch(FetchServiceConfig {
                validator: ValidatorConfig {
                    config: zebra_state::Config::default(),
                    rpc_address: cfg.validator_listen_address,
                    indexer_rpc_address: cfg.validator_grpc_listen_address,
                    // todo! the whole IndexerConfig struct should also be refactored into sub-structs and all this can be avoided
                    cookie: {
                        if cfg.enable_cookie_auth {
                            CookieAuth::Enabled {
                                path: cfg
                                    .cookie_dir
                                    .expect("should provide cookie dir if enable_cookie_auth=true"),
                            }
                        } else {
                            CookieAuth::Disabled
                        }
                    },
                    rpc_user: cfg.validator_user.unwrap_or_else(|| "xxxxxx".to_string()),
                    rpc_password: cfg
                        .validator_password
                        .unwrap_or_else(|| "xxxxxx".to_string()),
                },
                service: ServiceConfig {
                    timeout: 30,
                    channel_size: 32,
                },
                block_cache: BlockCacheConfig {
                    cache: CacheConfig {
                        capacity: cfg.map_capacity,
                        shard_amount: cfg.map_shard_amount,
                    },
                    database: DatabaseConfig {
                        path: cfg.zaino_db_path,
                        size: cfg.db_size,
                    },
                    network,
                    no_sync: cfg.no_sync,
                    no_db: cfg.no_db,
                },
            })),
        }
    }
}

/// Custom serializer for BackendType
fn serialize_backendtype_to_string<S>(
    backend_type: &BackendType,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(match backend_type {
        BackendType::State => "state",
        BackendType::Fetch => "fetch",
    })
}
