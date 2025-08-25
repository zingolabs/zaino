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
    BackendConfig, DebugConfig, GrpcConfig, JsonRpcAuth, JsonRpcConfig, Network, ServiceConfig,
    StorageConfig, TlsConfig, ZebradStateConfig,
};
use zaino_fetch::config::FetchServiceConfig;
use zaino_state::StateServiceConfig;

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

/// Server configuration for Zaino's own servers (JSON-RPC and gRPC).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// JSON-RPC server configuration.
    ///
    /// Set to `None` to completely disable the JSON-RPC server.
    /// Set to `Some(config)` to enable the JSON-RPC server with the specified configuration.
    pub json_rpc: Option<JsonRpcConfig>,

    /// gRPC server configuration.
    ///
    /// The gRPC server is always enabled and required for Zaino operation.
    pub grpc: GrpcConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // JSON-RPC server is disabled by default
            json_rpc: None,
            // gRPC server is always enabled with default settings
            grpc: GrpcConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Validates TLS configuration for gRPC server.
    pub fn validate_tls(&self) -> Result<(), IndexerError> {
        match self.grpc.tls {
            TlsConfig::Enabled {
                ref cert_path,
                ref key_path,
            } => {
                if !cert_path.exists() {
                    return Err(IndexerError::ConfigError(format!(
                        "TLS is enabled, but certificate path '{}' does not exist.",
                        cert_path.display()
                    )));
                }
                if !key_path.exists() {
                    return Err(IndexerError::ConfigError(format!(
                        "TLS is enabled, but key path '{}' does not exist.",
                        key_path.display()
                    )));
                }
            }
            TlsConfig::Disabled => {
                // TLS is disabled, no validation needed
            }
        }
        Ok(())
    }

    /// Validates that gRPC and JSON-RPC servers don't conflict.
    pub fn validate_server_addresses(&self) -> Result<(), IndexerError> {
        if let Some(ref json_rpc_config) = self.json_rpc {
            if json_rpc_config.listen_address == self.grpc.listen_address {
                return Err(IndexerError::ConfigError(
                    "gRPC server and JsonRPC server must listen on different addresses."
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Validates network security requirements for external access.
    #[cfg(not(feature = "disable_tls_unencrypted_traffic_mode"))]
    pub fn validate_network_security(&self) -> Result<(), IndexerError> {
        let grpc_addr = fetch_socket_addr_from_hostname(&self.grpc.listen_address.to_string())?;
        let grpc_tls_enabled = matches!(self.grpc.tls, TlsConfig::Enabled { .. });

        if !is_private_listen_addr(&grpc_addr) && !grpc_tls_enabled {
            return Err(IndexerError::ConfigError(
                "TLS required when connecting to external addresses.".to_string(),
            ));
        }
        Ok(())
    }

    /// No-op for TLS disabled mode.
    #[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
    pub fn validate_network_security(&self) -> Result<(), IndexerError> {
        Ok(())
    }
}

/// Config information required for Zaino.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IndexerConfig {
    /// Network type (Mainnet, Testnet, Regtest).
    pub network: Network,
    /// Server configuration (zaino's own JSON-RPC and gRPC servers).
    pub server: ServerConfig,
    /// Backend configuration (validator and backend type).
    pub backend: BackendConfig,
    /// Service-level configuration.
    pub service: ServiceConfig,
    /// Storage configuration (cache and database).
    pub storage: StorageConfig,
    /// Debug and testing configuration.
    pub debug: DebugConfig,
}

impl IndexerConfig {
    /// Performs checks on config data.
    pub fn check_config(&self) -> Result<(), IndexerError> {
        // Validate server configuration
        self.server.validate_tls()?;
        self.server.validate_server_addresses()?;
        self.server.validate_network_security()?;

        // Validate backend configuration
        self.backend
            .validate_auth()
            .map_err(|e| IndexerError::ConfigError(e.to_string()))?;

        // Validate backend network security
        let validator_addr =
            fetch_socket_addr_from_hostname(&self.backend.rpc_address().to_string())?;
        if !is_private_listen_addr(&validator_addr) {
            return Err(IndexerError::ConfigError(
                "Zaino may only connect to validator with private IP addresses.".to_string(),
            ));
        }

        // Additional backend-specific validation for non-loopback addresses
        #[cfg(not(feature = "disable_tls_unencrypted_traffic_mode"))]
        {
            // todo!: move to BackendConfig method
            if !is_loopback_listen_addr(&validator_addr) {
                match &self.backend {
                    BackendConfig::LocalZebra { auth, .. }
                    | BackendConfig::RemoteZebra { auth, .. } => {
                        if matches!(auth, zaino_commons::config::ZebradAuth::Disabled) {
                            return Err(IndexerError::ConfigError(
                                "Validator listen address is not loopback, so authentication must be enabled."
                                    .to_string(),
                            ));
                        }
                    }
                    BackendConfig::RemoteZcashd { auth, .. } => {
                        if matches!(auth, zaino_commons::config::ZcashdAuth::Disabled) {
                            return Err(IndexerError::ConfigError(
                                "Validator listen address is not loopback, so authentication must be enabled."
                                    .to_string(),
                            ));
                        }
                    }
                    BackendConfig::RemoteZainod { auth, .. } => {
                        if matches!(auth, zaino_commons::config::ZcashdAuth::Disabled) {
                            return Err(IndexerError::ConfigError(
                                "Validator listen address is not loopback, so authentication must be enabled."
                                    .to_string(),
                            ));
                        }
                    }
                }
            }
        }

        #[cfg(feature = "disable_tls_unencrypted_traffic_mode")]
        {
            warn!("Zaino built using disable_tls_unencrypted_traffic_mode feature, proceed with caution.");
        }

        Ok(())
    }

    /// Finalizes the configuration after initial parsing, applying conditional defaults.
    fn finalize_config_logic(mut self) -> Self {
        // Ensure cookie path is set for enabled cookie auth in JSON-RPC server config
        if let Some(ref mut json_rpc_config) = self.server.json_rpc {
            if let JsonRpcAuth::Cookie(ref mut cookie_auth) = json_rpc_config.auth {
                if cookie_auth.path.as_os_str().is_empty() {
                    cookie_auth.path = default_ephemeral_cookie_path();
                }
            }
        }
        self
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            network: Network::Testnet,
            server: ServerConfig::default(),
            backend: BackendConfig::default(),
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
/// If the file cannot be read, or if its contents cannot be parsed into `ZainoConfig`,
/// a warning is logged, and a default configuration is returned.
/// The loaded or default configuration undergoes further checks and finalization.
pub fn load_config(file_path: &PathBuf) -> Result<IndexerConfig, IndexerError> {
    // Configuration sources are layered: Env > TOML > Defaults.
    let figment = Figment::new()
        // 1. Base defaults from `ZainoConfig::default()`.
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

/// Creates service configurations from ZainoConfig.
impl IndexerConfig {
    /// Creates StateService configuration from IndexerConfig.
    ///
    /// Returns an error if called on a non-LocalZebra backend.
    pub fn to_state_service_config(&self) -> Result<StateServiceConfig, IndexerError> {
        match &self.backend {
            BackendConfig::LocalZebra {
                rpc_address,
                auth,
                zebra_state,
                indexer_rpc_address,
                zebra_database,
            } => Ok(StateServiceConfig {
                zebra: ZebradStateConfig {
                    rpc_address: *rpc_address,
                    auth: auth.clone(),
                    state: zebra_state.clone(),
                    indexer_rpc_address: *indexer_rpc_address,
                    database: zebra_database.clone(),
                },
                service: self.service.clone(),
                storage: self.storage.clone(),
                network: self.network,
                debug: self.debug.clone(),
            }),
            _ => Err(IndexerError::ConfigError(
                "Cannot create StateService config from remote backend. Only LocalZebra backend supports StateService.".to_string()
            )),
        }
    }

    /// Creates FetchService configuration from IndexerConfig.
    ///
    /// Returns an error if called on a LocalZebra backend.
    pub fn to_fetch_service_config(&self) -> Result<FetchServiceConfig, IndexerError> {
        match &self.backend {
            BackendConfig::RemoteZebra { rpc_address, auth } => {
                Ok(FetchServiceConfig {
                    validator: zaino_commons::config::JsonRpcValidatorConfig::Zebrd { rpc_address: *rpc_address, auth: auth.clone() },
                    service: self.service.clone(),
                    storage: self.storage.clone(),
                    network: self.network,
                    debug: self.debug.clone()
                })
            },
            BackendConfig::RemoteZcashd { rpc_address, auth } => {
                Ok(FetchServiceConfig {
                    validator: zaino_commons::config::JsonRpcValidatorConfig::Zcashd { rpc_address: *rpc_address, auth: auth.clone() },
                    service: self.service.clone(),
                    storage: self.storage.clone(),
                    network: self.network,
                    debug: self.debug.clone()
                })
            },
            BackendConfig::RemoteZainod { rpc_address, auth } => {
                Ok(FetchServiceConfig {
                    validator: zaino_commons::config::JsonRpcValidatorConfig::Zcashd { rpc_address: *rpc_address, auth: auth.clone() },
                    service: self.service.clone(),
                    storage: self.storage.clone(),
                    network: self.network,
                    debug: self.debug.clone()
                })
            },
            BackendConfig::LocalZebra { .. } => {
                Err(IndexerError::ConfigError(
                    "Cannot create FetchService config from LocalZebra backend. Use StateService instead.".to_string()
                ))
            }
        }
    }
}
