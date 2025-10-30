//! Zaino config.
use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
};
// Added for Serde deserialization helpers
use crate::error::IndexerError;
use serde::{
    de::{self, Deserializer},
    Deserialize, Serialize,
};
#[cfg(feature = "no_tls_use_unencrypted_traffic")]
use tracing::warn;
use tracing::{error, info};
use zaino_common::{
    CacheConfig, DatabaseConfig, DatabaseSize, Network, ServiceConfig, StorageConfig,
    ValidatorConfig,
};
use zaino_serve::server::config::{GrpcServerConfig, JsonRpcServerConfig};

#[allow(deprecated)]
use zaino_state::{BackendConfig, FetchServiceConfig, StateServiceConfig};

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
pub struct ZainodConfig {
    /// Type of backend to be used.
    #[serde(deserialize_with = "deserialize_backendtype_from_string")]
    #[serde(serialize_with = "serialize_backendtype_to_string")]
    pub backend: zaino_state::BackendType,
    /// Enable JsonRPC server with a valid Some value.
    #[serde(default)]
    pub json_server_settings: Option<JsonRpcServerConfig>,
    /// gRPC server settings including listen addr, tls status, key and cert.
    pub grpc_settings: GrpcServerConfig,
    /// Full node / validator configuration settings.
    pub validator_settings: ValidatorConfig,
    /// Service-level configuration (timeout, channel size).
    pub service: ServiceConfig,
    /// Storage configuration (cache and database).
    pub storage: StorageConfig,
    /// Block Cache database file path.
    ///
    /// ZebraDB location.
    pub zebra_db_path: PathBuf,
    /// Network chain type.
    pub network: Network,
}

impl ZainodConfig {
    /// Performs checks on config data.
    pub(crate) fn check_config(&self) -> Result<(), IndexerError> {
        // Network type is validated at the type level via Network enum.
        // Check TLS settings.
        if self.grpc_settings.tls.is_some() {
            // then check if cert path exists or return error
            let c_path = &self
                .grpc_settings
                .tls
                .as_ref()
                .expect("to be Some")
                .cert_path;
            if !std::path::Path::new(&c_path).exists() {
                return Err(IndexerError::ConfigError(format!(
                    "TLS is enabled, but certificate path {:?} does not exist.",
                    c_path
                )));
            }

            let k_path = &self
                .grpc_settings
                .tls
                .as_ref()
                .expect("to be Some")
                .key_path;
            if !std::path::Path::new(&k_path).exists() {
                return Err(IndexerError::ConfigError(format!(
                    "TLS is enabled, but key path {:?} does not exist.",
                    k_path
                )));
            }
        }

        // Check validator cookie authentication settings
        if self.validator_settings.validator_cookie_path.is_some() {
            if let Some(ref cookie_path) = self.validator_settings.validator_cookie_path {
                if !std::path::Path::new(cookie_path).exists() {
                    return Err(IndexerError::ConfigError(
                        format!("Validator cookie authentication is enabled, but cookie path '{:?}' does not exist.", cookie_path),
                    ));
                }
            } else {
                return Err(IndexerError::ConfigError(
                    "Validator cookie authentication is enabled, but no cookie path is provided."
                        .to_string(),
                ));
            }
        }

        #[cfg(not(feature = "no_tls_use_unencrypted_traffic"))]
        let grpc_addr =
            fetch_socket_addr_from_hostname(&self.grpc_settings.listen_address.to_string())?;

        let validator_addr = fetch_socket_addr_from_hostname(
            &self
                .validator_settings
                .validator_jsonrpc_listen_address
                .to_string(),
        )?;

        // Ensure validator listen address is private.
        if !is_private_listen_addr(&validator_addr) {
            return Err(IndexerError::ConfigError(
                "Zaino may only connect to Zebra with private IP addresses.".to_string(),
            ));
        }

        #[cfg(not(feature = "no_tls_use_unencrypted_traffic"))]
        {
            // Ensure TLS is used when connecting to external addresses.
            if !is_private_listen_addr(&grpc_addr) && self.grpc_settings.tls.is_none() {
                return Err(IndexerError::ConfigError(
                    "TLS required when connecting to external addresses.".to_string(),
                ));
            }

            // Ensure validator rpc cookie authentication is used when connecting to non-loopback addresses.
            if !is_loopback_listen_addr(&validator_addr)
                && self.validator_settings.validator_cookie_path.is_none()
            {
                return Err(IndexerError::ConfigError(
                "Validator listen address is not loopback, so cookie authentication must be enabled."
                    .to_string(),
            ));
            }
        }

        #[cfg(feature = "no_tls_use_unencrypted_traffic")]
        {
            warn!(
                "Zaino built using no_tls_use_unencrypted_traffic feature, proceed with caution."
            );
        }

        // Check gRPC and JsonRPC server are not listening on the same address.
        if self.json_server_settings.is_some()
            && self
                .json_server_settings
                .as_ref()
                .expect("json_server_settings to be Some")
                .json_rpc_listen_address
                == self.grpc_settings.listen_address
        {
            return Err(IndexerError::ConfigError(
                "gRPC server and JsonRPC server must listen on different addresses.".to_string(),
            ));
        }

        Ok(())
    }

    /// Returns the network type currently being used by the server.
    pub fn get_network(&self) -> Result<zebra_chain::parameters::Network, IndexerError> {
        Ok(self.network.to_zebra_network())
    }
}

impl Default for ZainodConfig {
    fn default() -> Self {
        Self {
            backend: zaino_state::BackendType::Fetch,
            json_server_settings: None,
            grpc_settings: GrpcServerConfig {
                listen_address: "127.0.0.1:8137".parse().unwrap(),
                tls: None,
            },
            validator_settings: ValidatorConfig {
                validator_grpc_listen_address: "127.0.0.1:18230".parse().unwrap(),
                validator_jsonrpc_listen_address: "127.0.0.1:18232".parse().unwrap(),
                validator_cookie_path: None,
                validator_user: Some("xxxxxx".to_string()),
                validator_password: Some("xxxxxx".to_string()),
            },
            service: ServiceConfig::default(),
            storage: StorageConfig {
                cache: CacheConfig::default(),
                database: DatabaseConfig {
                    path: default_zaino_db_path(),
                    size: DatabaseSize::default(),
                },
            },
            zebra_db_path: default_zebra_db_path().unwrap(),
            network: Network::Testnet,
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
#[cfg_attr(feature = "no_tls_use_unencrypted_traffic", allow(dead_code))]
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
/// Finally, there is an override of the config using environmental variables.
/// The loaded or default configuration undergoes further checks and finalization.
pub fn load_config(file_path: &PathBuf) -> Result<ZainodConfig, IndexerError> {
    // Configuration sources are layered: Env > TOML > Defaults.
    let figment = Figment::new()
        // 1. Base defaults from `IndexerConfig::default()`.
        .merge(Serialized::defaults(ZainodConfig::default()))
        // 2. Override with values from the TOML configuration file.
        .merge(Toml::file(file_path))
        // 3. Override with values from environment variables prefixed with "ZAINO_".
        .merge(figment::providers::Env::prefixed("ZAINO_").split("-"));

    match figment.extract::<ZainodConfig>() {
        Ok(mut parsed_config) => {
            if parsed_config
                .json_server_settings
                .clone()
                .is_some_and(|json_settings| {
                    json_settings.cookie_dir.is_some()
                        && json_settings
                            .cookie_dir
                            .expect("cookie_dir to be Some")
                            .as_os_str()
                            // if the assigned pathbuf is empty (cookies enabled but no path defined).
                            .is_empty()
                })
            {
                if let Some(ref mut json_config) = parsed_config.json_server_settings {
                    json_config.cookie_dir = Some(default_ephemeral_cookie_path());
                }
            };

            parsed_config.check_config()?;
            info!(
                "Successfully loaded and validated config. Base TOML file checked: '{}'",
                file_path.display()
            );
            Ok(parsed_config)
        }
        Err(figment_error) => {
            error!(
                "Failed to extract configuration using figment: {}",
                figment_error
            );
            Err(IndexerError::ConfigError(format!(
                "Zaino configuration loading failed during figment extract '{}' (could be TOML file or environment variables). Details: {}",
                file_path.display(), figment_error
            )))
        }
    }
}

impl TryFrom<ZainodConfig> for BackendConfig {
    type Error = IndexerError;

    #[allow(deprecated)]
    fn try_from(cfg: ZainodConfig) -> Result<Self, Self::Error> {
        match cfg.backend {
            zaino_state::BackendType::State => Ok(BackendConfig::State(StateServiceConfig {
                validator_state_config: zebra_state::Config {
                    cache_dir: cfg.zebra_db_path.clone(),
                    ephemeral: false,
                    delete_old_database: true,
                    debug_stop_at_height: None,
                    debug_validity_check_interval: None,
                },
                validator_rpc_address: cfg.validator_settings.validator_jsonrpc_listen_address,
                validator_grpc_address: cfg.validator_settings.validator_grpc_listen_address,
                validator_cookie_auth: cfg.validator_settings.validator_cookie_path.is_some(),
                validator_cookie_path: cfg.validator_settings.validator_cookie_path,
                validator_rpc_user: cfg
                    .validator_settings
                    .validator_user
                    .unwrap_or_else(|| "xxxxxx".to_string()),
                validator_rpc_password: cfg
                    .validator_settings
                    .validator_password
                    .unwrap_or_else(|| "xxxxxx".to_string()),
                service: cfg.service,
                storage: cfg.storage,
                network: cfg.network,
            })),

            zaino_state::BackendType::Fetch => Ok(BackendConfig::Fetch(FetchServiceConfig {
                validator_rpc_address: cfg.validator_settings.validator_jsonrpc_listen_address,
                validator_cookie_path: cfg.validator_settings.validator_cookie_path,
                validator_rpc_user: cfg
                    .validator_settings
                    .validator_user
                    .unwrap_or_else(|| "xxxxxx".to_string()),
                validator_rpc_password: cfg
                    .validator_settings
                    .validator_password
                    .unwrap_or_else(|| "xxxxxx".to_string()),
                service: cfg.service,
                storage: cfg.storage,
                network: cfg.network,
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
