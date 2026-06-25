//! Zaino config.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

/// Default port for the Prometheus metrics endpoint.
pub const DEFAULT_METRICS_PORT: u16 = 9998;

use serde::{Deserialize, Serialize};
use tracing::info;
#[cfg(any(
    feature = "no_tls_use_unencrypted_traffic",
    feature = "allow_unencrypted_public_json_rpc_bind"
))]
use tracing::warn;

use crate::error::IndexerError;
use zaino_common::{
    try_resolve_address, AddressResolution, Network, ServiceConfig, StorageConfig, ValidatorConfig,
};
use zaino_serve::server::config::{GrpcServerConfig, JsonRpcServerConfig};
#[allow(deprecated)]
use zaino_state::{
    BackendType, CommonBackendConfig, DonationAddress, FetchServiceConfig, StateServiceConfig,
};

/// Header for generated configuration files.
pub const GENERATED_CONFIG_HEADER: &str = r#"# Zaino Configuration
#
# Generated with `zainod generate-config`
#
# Configuration sources are layered (highest priority first):
#   1. Environment variables (prefix: ZAINO_)
#   2. TOML configuration file
#   3. Built-in defaults
#
# For detailed documentation, see:
#   https://github.com/zingolabs/zaino

"#;

/// Generate default configuration file content.
///
/// Returns the full config file content including header and TOML-serialized defaults.
pub fn generate_default_config() -> Result<String, IndexerError> {
    let config = ZainodConfig::default();

    let toml_content = toml::to_string_pretty(&config)
        .map_err(|e| IndexerError::ConfigError(format!("Failed to serialize config: {}", e)))?;

    Ok(format!("{}{}", GENERATED_CONFIG_HEADER, toml_content))
}

/// Sensitive key suffixes that should not be set via environment variables.
const SENSITIVE_KEY_SUFFIXES: [&str; 5] = ["password", "secret", "token", "cookie", "private_key"];

/// Checks if a key is sensitive and should not be set via environment variables.
fn is_sensitive_leaf_key(leaf_key: &str) -> bool {
    let key = leaf_key.to_ascii_lowercase();
    SENSITIVE_KEY_SUFFIXES
        .iter()
        .any(|suffix| key.ends_with(suffix))
}

/// Zaino daemon configuration.
///
/// Field order matters for TOML serialization: simple values must come before tables.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ZainodConfig {
    // Simple values first (TOML requirement)
    /// Backend type for fetching blockchain data.
    pub backend: BackendType,
    /// Path to Zebra's state database.
    ///
    /// Required when using the `state` backend.
    pub zebra_db_path: PathBuf,
    /// Run the finalised-state database in ephemeral/stateless mode.
    ///
    /// When enabled, Zaino does not use a persistent on-disk finalised-state database. Finalised
    /// state reads are served from the configured validator/source instead.
    pub ephemeral_finalised_state: bool,
    /// Network to connect to (Mainnet, Testnet, or Regtest).
    pub network: Network,
    /// Prometheus metrics endpoint listen address.
    ///
    /// Set to enable the `/metrics` scrape endpoint. Disabled when `None`.
    /// Requires the `prometheus` feature; ignored without it.
    pub metrics_endpoint: Option<SocketAddr>,

    // Table sections
    /// JSON-RPC server settings. Set to enable Zaino's JSON-RPC interface.
    pub json_server_settings: Option<JsonRpcServerConfig>,
    /// gRPC server settings (listen address, TLS configuration).
    pub grpc_settings: GrpcServerConfig,
    /// Validator connection settings.
    pub validator_settings: ValidatorConfig,
    /// Service-level settings (timeout, channel size).
    pub service: ServiceConfig,
    /// Storage settings (cache and database).
    pub storage: StorageConfig,
    /// Zcash donation UA address
    pub donation_address: Option<DonationAddress>,
}

impl ZainodConfig {
    /// Performs checks on config data.
    pub(crate) fn check_config(&self) -> Result<(), IndexerError> {
        // Check TLS settings.
        if let Some(ref tls) = self.grpc_settings.tls {
            if !std::path::Path::new(&tls.cert_path).exists() {
                return Err(IndexerError::ConfigError(format!(
                    "TLS is enabled, but certificate path {:?} does not exist.",
                    tls.cert_path
                )));
            }

            if !std::path::Path::new(&tls.key_path).exists() {
                return Err(IndexerError::ConfigError(format!(
                    "TLS is enabled, but key path {:?} does not exist.",
                    tls.key_path
                )));
            }
        }

        // Check validator cookie authentication settings
        if let Some(ref cookie_path) = self.validator_settings.validator_cookie_path {
            if !std::path::Path::new(cookie_path).exists() {
                return Err(IndexerError::ConfigError(format!(
                    "Validator cookie authentication is enabled, but cookie path '{:?}' does not exist.",
                    cookie_path
                )));
            }
        }

        #[cfg(not(feature = "no_tls_use_unencrypted_traffic"))]
        let grpc_addr =
            fetch_socket_addr_from_hostname(&self.grpc_settings.listen_address.to_string())?;

        // Validate the validator address using the richer result type that distinguishes
        // between format errors (always fail) and DNS lookup failures (can defer for Docker).
        let validator_addr_result =
            try_resolve_address(&self.validator_settings.validator_jsonrpc_listen_address);

        // Validator address validation:
        // - Resolved IPs: must be private (RFC1918/ULA)
        // - Hostnames: validated at connection time (supports Docker/K8s service discovery)
        // - Cookie auth: determined by validator_cookie_path config, not enforced by address type
        match validator_addr_result {
            AddressResolution::Resolved(validator_addr) => {
                if !is_private_listen_addr(&validator_addr) {
                    return Err(IndexerError::ConfigError(
                        "Zaino may only connect to Zebra with private IP addresses.".to_string(),
                    ));
                }
            }
            AddressResolution::UnresolvedHostname { ref address, .. } => {
                info!(
                    "Validator address '{}' cannot be resolved at config time.",
                    address
                );
            }
            AddressResolution::InvalidFormat { address, reason } => {
                // Invalid address format - always fail immediately.
                return Err(IndexerError::ConfigError(format!(
                    "Invalid validator address '{}': {}",
                    address, reason
                )));
            }
        }

        #[cfg(not(feature = "no_tls_use_unencrypted_traffic"))]
        {
            // Ensure TLS is used when connecting to external addresses.
            if !is_private_listen_addr(&grpc_addr) && self.grpc_settings.tls.is_none() {
                return Err(IndexerError::ConfigError(
                    "TLS required when connecting to external addresses.".to_string(),
                ));
            }
        }

        #[cfg(feature = "no_tls_use_unencrypted_traffic")]
        {
            warn!(
                "Zaino built using no_tls_use_unencrypted_traffic feature, proceed with caution."
            );
        }

        // The JSON-RPC interface is unencrypted and intended for loopback / trusted
        // private networks only. Reject public bind addresses unless explicitly unlocked.
        #[cfg(not(feature = "allow_unencrypted_public_json_rpc_bind"))]
        if let Some(ref json_settings) = self.json_server_settings {
            if !is_private_listen_addr(&json_settings.json_rpc_listen_address) {
                return Err(IndexerError::ConfigError(
                    "JSON-RPC server may only bind to private or loopback addresses. \
                     Build with the `allow_unencrypted_public_json_rpc_bind` feature to \
                     override (trusted private networks only)."
                        .to_string(),
                ));
            }
        }

        #[cfg(feature = "allow_unencrypted_public_json_rpc_bind")]
        {
            warn!(
                "Zaino built with allow_unencrypted_public_json_rpc_bind: the JSON-RPC \
                 server may bind to public addresses without encryption. Proceed with caution."
            );
        }

        // Check gRPC and JsonRPC server are not listening on the same address.
        if let Some(ref json_settings) = self.json_server_settings {
            if json_settings.json_rpc_listen_address == self.grpc_settings.listen_address {
                return Err(IndexerError::ConfigError(
                    "gRPC server and JsonRPC server must listen on different addresses."
                        .to_string(),
                ));
            }
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
            backend: BackendType::default(),
            metrics_endpoint: None,
            json_server_settings: None,
            grpc_settings: GrpcServerConfig {
                listen_address: "127.0.0.1:8137".parse().unwrap(),
                tls: None,
            },
            validator_settings: ValidatorConfig {
                validator_grpc_listen_address: Some("127.0.0.1:18230".to_string()),
                validator_jsonrpc_listen_address: "127.0.0.1:18232".to_string(),
                validator_cookie_path: None,
                validator_user: Some("xxxxxx".to_string()),
                validator_password: Some("xxxxxx".to_string()),
            },
            service: ServiceConfig::default(),
            storage: StorageConfig::default(),
            ephemeral_finalised_state: false,
            zebra_db_path: default_zebra_db_path(),
            network: Network::Testnet,
            donation_address: None,
        }
    }
}

/// Returns the default path for Zaino's ephemeral authentication cookie.
pub fn default_ephemeral_cookie_path() -> PathBuf {
    zaino_common::xdg::resolve_path_with_xdg_runtime_defaults("zaino/.cookie")
}

/// Loads the default file path for zebra's local db.
pub fn default_zebra_db_path() -> PathBuf {
    zaino_common::xdg::resolve_path_with_xdg_cache_defaults("zebra")
}

/// Resolves a hostname to a SocketAddr.
fn fetch_socket_addr_from_hostname(address: &str) -> Result<SocketAddr, IndexerError> {
    zaino_common::net::resolve_socket_addr(address)
        .map_err(|e| IndexerError::ConfigError(format!("Invalid address '{address}': {e}")))
}

/// Validates that the configured `address` is either:
/// - An RFC1918 (private) IPv4 address, or
/// - An IPv6 Unique Local Address (ULA)
pub(crate) fn is_private_listen_addr(addr: &SocketAddr) -> bool {
    let ip = addr.ip();
    match ip {
        IpAddr::V4(ipv4) => ipv4.is_private() || ipv4.is_loopback(),
        IpAddr::V6(ipv6) => ipv6.is_unique_local() || ip.is_loopback(),
    }
}

/// Loads configuration from a TOML file with optional environment variable overrides.
///
/// Configuration is layered: Defaults → TOML file → Environment variables (prefix: ZAINO_).
/// Sensitive keys (password, secret, token, cookie, private_key) are blocked from env vars.
pub fn load_config(file_path: &std::path::Path) -> Result<ZainodConfig, IndexerError> {
    load_config_with_env(file_path, "ZAINO")
}

/// Loads configuration with a custom environment variable prefix.
pub fn load_config_with_env(
    file_path: &std::path::Path,
    env_prefix: &str,
) -> Result<ZainodConfig, IndexerError> {
    // Check for sensitive keys in environment variables before loading
    let required_prefix = format!("{}_", env_prefix);
    for (key, _) in std::env::vars() {
        if let Some(without_prefix) = key.strip_prefix(&required_prefix) {
            if let Some(leaf) = without_prefix.split("__").last() {
                if is_sensitive_leaf_key(leaf) {
                    return Err(IndexerError::ConfigError(format!(
                        "Environment variable '{}' contains sensitive key '{}' - use config file instead",
                        key, leaf
                    )));
                }
            }
        }
    }

    let mut builder = config::Config::builder()
        .set_default("backend", "fetch")
        .map_err(|e| IndexerError::ConfigError(e.to_string()))?;

    // Add TOML file source
    builder = builder.add_source(
        config::File::from(file_path)
            .format(config::FileFormat::Toml)
            .required(true),
    );

    // Add environment variable source with ZAINO_ prefix and __ separator for nesting
    // Note: config-rs lowercases all env var keys after stripping the prefix
    builder = builder.add_source(
        config::Environment::with_prefix(env_prefix)
            .prefix_separator("_")
            .separator("__")
            .try_parsing(true),
    );

    let settings = builder
        .build()
        .map_err(|e| IndexerError::ConfigError(format!("Configuration loading failed: {}", e)))?;

    let mut parsed_config: ZainodConfig = settings
        .try_deserialize()
        .map_err(|e| IndexerError::ConfigError(format!("Configuration parsing failed: {}", e)))?;

    // Handle empty cookie_dir: if json_server_settings exists with empty cookie_dir, set default
    if parsed_config
        .json_server_settings
        .as_ref()
        .is_some_and(|json_settings| {
            json_settings
                .cookie_dir
                .as_ref()
                .is_some_and(|dir| dir.as_os_str().is_empty())
        })
    {
        if let Some(ref mut json_config) = parsed_config.json_server_settings {
            json_config.cookie_dir = Some(default_ephemeral_cookie_path());
        }
    }

    parsed_config.check_config()?;
    info!(
        "Successfully loaded and validated config. Base TOML file checked: '{}'",
        file_path.display()
    );
    Ok(parsed_config)
}

#[allow(deprecated)]
impl TryFrom<ZainodConfig> for StateServiceConfig {
    type Error = IndexerError;

    fn try_from(cfg: ZainodConfig) -> Result<Self, Self::Error> {
        let grpc_listen_address = cfg
            .validator_settings
            .validator_grpc_listen_address
            .as_ref()
            .ok_or_else(|| {
                IndexerError::ConfigError(
                    "Missing validator_grpc_listen_address in configuration".to_string(),
                )
            })?;

        let validator_grpc_address =
            fetch_socket_addr_from_hostname(grpc_listen_address).map_err(|e| {
                let msg = match e {
                    IndexerError::ConfigError(msg) => msg,
                    other => other.to_string(),
                };
                IndexerError::ConfigError(format!(
                    "Invalid validator_grpc_listen_address '{grpc_listen_address}': {msg}"
                ))
            })?;

        let validator_state_config = zebra_state::Config {
            cache_dir: cfg.zebra_db_path.clone(),
            ephemeral: false,
            delete_old_database: true,
            debug_stop_at_height: None,
            debug_validity_check_interval: None,
            should_backup_non_finalized_state: true,
            debug_skip_non_finalized_state_backup_task: false,
        };
        let validator_cookie_auth = cfg.validator_settings.validator_cookie_path.is_some();

        Ok(StateServiceConfig {
            common: build_common(cfg),
            validator_state_config,
            validator_grpc_address,
            validator_cookie_auth,
        })
    }
}

#[allow(deprecated)]
impl TryFrom<ZainodConfig> for FetchServiceConfig {
    type Error = IndexerError;

    fn try_from(cfg: ZainodConfig) -> Result<Self, Self::Error> {
        Ok(FetchServiceConfig {
            common: build_common(cfg),
        })
    }
}

fn build_common(cfg: ZainodConfig) -> CommonBackendConfig {
    CommonBackendConfig {
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
        ephemeral_finalised_state: cfg.ephemeral_finalised_state,
        network: cfg.network,
        donation_address: cfg.donation_address,
        indexer_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, sync::Mutex};
    use tempfile::TempDir;

    const ZAINO_ENV_PREFIX: &str = "ZAINO_";
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    /// RAII guard for managing environment variables in tests.
    /// Ensures test isolation by clearing ZAINO_* vars before tests
    /// and restoring original values after.
    struct EnvGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        original_vars: Vec<(String, String)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let original_vars: Vec<_> = env::vars()
                .filter(|(k, _)| k.starts_with(ZAINO_ENV_PREFIX))
                .collect();
            // Clear all ZAINO_* vars for test isolation
            for (key, _) in &original_vars {
                env::remove_var(key);
            }
            Self {
                _guard: guard,
                original_vars,
            }
        }

        fn set_var(&self, key: &str, value: &str) {
            env::set_var(key, value);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Clear test vars
            for (k, _) in env::vars().filter(|(k, _)| k.starts_with(ZAINO_ENV_PREFIX)) {
                env::remove_var(&k);
            }
            // Restore originals
            for (k, v) in &self.original_vars {
                env::set_var(k, v);
            }
        }
    }

    fn create_test_config_file(dir: &TempDir, content: &str, filename: &str) -> PathBuf {
        let path = dir.path().join(filename);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_deserialize_full_valid_config() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        // Create mock files
        let cert_file = temp_dir.path().join("test_cert.pem");
        let key_file = temp_dir.path().join("test_key.pem");
        let validator_cookie_file = temp_dir.path().join("validator.cookie");
        let zaino_cookie_dir = temp_dir.path().join("zaino_cookies_dir");
        let zaino_db_dir = temp_dir.path().join("zaino_db_dir");
        let zebra_db_dir = temp_dir.path().join("zebra_db_dir");

        std::fs::write(&cert_file, "mock cert content").unwrap();
        std::fs::write(&key_file, "mock key content").unwrap();
        std::fs::write(&validator_cookie_file, "mock validator cookie content").unwrap();
        std::fs::create_dir_all(&zaino_cookie_dir).unwrap();
        std::fs::create_dir_all(&zaino_db_dir).unwrap();
        std::fs::create_dir_all(&zebra_db_dir).unwrap();

        let toml_content = format!(
            r#"
backend = "fetch"
zebra_db_path = "{}"
network = "Mainnet"

[storage.database]
path = "{}"

[validator_settings]
validator_jsonrpc_listen_address = "192.168.1.10:18232"
validator_cookie_path = "{}"
validator_user = "user"
validator_password = "password"

[json_server_settings]
json_rpc_listen_address = "127.0.0.1:8000"
cookie_dir = "{}"

[grpc_settings]
listen_address = "0.0.0.0:9000"

[grpc_settings.tls]
cert_path = "{}"
key_path = "{}"
"#,
            zebra_db_dir.display(),
            zaino_db_dir.display(),
            validator_cookie_file.display(),
            zaino_cookie_dir.display(),
            cert_file.display(),
            key_file.display(),
        );

        let config_path = create_test_config_file(&temp_dir, &toml_content, "full_config.toml");
        let config = load_config(&config_path).expect("load_config failed");

        assert_eq!(config.backend, BackendType::Fetch);
        assert!(config.json_server_settings.is_some());
        assert_eq!(
            config
                .json_server_settings
                .as_ref()
                .unwrap()
                .json_rpc_listen_address,
            "127.0.0.1:8000".parse().unwrap()
        );
        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(
            config.grpc_settings.listen_address,
            "0.0.0.0:9000".parse().unwrap()
        );
        assert!(config.grpc_settings.tls.is_some());
        assert_eq!(
            config.validator_settings.validator_user,
            Some("user".to_string())
        );
        assert_eq!(
            config.validator_settings.validator_password,
            Some("password".to_string())
        );
    }

    #[test]
    fn test_deserialize_optional_fields_missing() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
backend = "state"
network = "Testnet"
zebra_db_path = "/opt/zebra/data"

[storage.database]
path = "/opt/zaino/data"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "optional_missing.toml");
        let config = load_config(&config_path).expect("load_config failed");
        let default_values = ZainodConfig::default();

        assert_eq!(config.backend, BackendType::State);
        assert!(config.json_server_settings.is_none());
        assert_eq!(
            config.validator_settings.validator_user,
            default_values.validator_settings.validator_user
        );
        assert_eq!(
            config.storage.cache.capacity,
            default_values.storage.cache.capacity
        );
    }

    #[test]
    fn test_cookie_dir_logic() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        // Scenario 1: auth enabled, cookie_dir empty (should use default ephemeral path)
        let toml_content = r#"
backend = "fetch"
network = "Testnet"
zebra_db_path = "/zebra/db"

[storage.database]
path = "/zaino/db"

[json_server_settings]
json_rpc_listen_address = "127.0.0.1:8237"
cookie_dir = ""

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "s1.toml");
        let config = load_config(&config_path).expect("Config S1 failed");
        assert!(config.json_server_settings.is_some());
        assert!(config
            .json_server_settings
            .as_ref()
            .unwrap()
            .cookie_dir
            .is_some());

        // Scenario 2: auth enabled, cookie_dir specified
        let toml_content2 = r#"
backend = "fetch"
network = "Testnet"
zebra_db_path = "/zebra/db"

[storage.database]
path = "/zaino/db"

[json_server_settings]
json_rpc_listen_address = "127.0.0.1:8237"
cookie_dir = "/my/cookie/path"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path2 = create_test_config_file(&temp_dir, toml_content2, "s2.toml");
        let config2 = load_config(&config_path2).expect("Config S2 failed");
        assert_eq!(
            config2.json_server_settings.as_ref().unwrap().cookie_dir,
            Some(PathBuf::from("/my/cookie/path"))
        );

        // Scenario 3: cookie_dir not specified (should be None)
        let toml_content3 = r#"
backend = "fetch"
network = "Testnet"
zebra_db_path = "/zebra/db"

[storage.database]
path = "/zaino/db"

[json_server_settings]
json_rpc_listen_address = "127.0.0.1:8237"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path3 = create_test_config_file(&temp_dir, toml_content3, "s3.toml");
        let config3 = load_config(&config_path3).expect("Config S3 failed");
        assert!(config3.json_server_settings.unwrap().cookie_dir.is_none());
    }

    #[test]
    fn test_deserialize_empty_string_yields_default() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        // Minimal valid config
        let toml_content = r#"
[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "empty.toml");
        let config = load_config(&config_path).expect("Empty TOML load failed");
        let default_config = ZainodConfig::default();

        assert_eq!(config.network, default_config.network);
        assert_eq!(config.backend, default_config.backend);
        assert_eq!(
            config.storage.cache.capacity,
            default_config.storage.cache.capacity
        );
    }

    #[test]
    fn test_deserialize_invalid_backend_type() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
backend = "invalid_type"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "invalid_backend.toml");
        let result = load_config(&config_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(
                msg.contains("unknown variant") || msg.contains("invalid_type"),
                "Unexpected error message: {}",
                msg
            );
        }
    }

    #[test]
    fn test_deserialize_invalid_socket_address() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
[json_server_settings]
json_rpc_listen_address = "not-a-valid-address"
cookie_dir = ""

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "invalid_socket.toml");
        let result = load_config(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_env_override_toml_and_defaults() {
        let guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
network = "Testnet"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        guard.set_var("ZAINO_NETWORK", "Mainnet");
        guard.set_var(
            "ZAINO_JSON_SERVER_SETTINGS__JSON_RPC_LISTEN_ADDRESS",
            "127.0.0.1:0",
        );
        guard.set_var("ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR", "/env/cookie/path");
        guard.set_var("ZAINO_STORAGE__CACHE__CAPACITY", "12345");
        guard.set_var("ZAINO_EPHEMERAL_FINALISED_STATE", "true");

        let config_path = create_test_config_file(&temp_dir, toml_content, "test_config.toml");
        let config = load_config(&config_path).expect("load_config should succeed");

        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.storage.cache.capacity, 12345);
        assert!(config.ephemeral_finalised_state);
        assert!(config.json_server_settings.is_some());
        assert_eq!(
            config.json_server_settings.as_ref().unwrap().cookie_dir,
            Some(PathBuf::from("/env/cookie/path"))
        );
    }

    #[test]
    fn test_toml_overrides_defaults() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        // json_server_settings without a listening address is forbidden
        let toml_content = r#"
network = "Regtest"

[json_server_settings]
json_rpc_listen_address = ""
cookie_dir = ""

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "test_config.toml");
        assert!(load_config(&config_path).is_err());
    }

    #[test]
    fn test_invalid_env_var_type() {
        let guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        guard.set_var("ZAINO_STORAGE__CACHE__CAPACITY", "not_a_number");

        let config_path = create_test_config_file(&temp_dir, toml_content, "test_config.toml");
        let result = load_config(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_cookie_auth_not_forced_for_non_loopback_ip() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
backend = "fetch"
network = "Testnet"

[validator_settings]
validator_jsonrpc_listen_address = "192.168.1.10:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "no_cookie_auth.toml");
        let config_result = load_config(&config_path);
        assert!(
            config_result.is_ok(),
            "Non-loopback IP without cookie auth should succeed. Error: {:?}",
            config_result.err()
        );

        let config = config_result.unwrap();
        assert!(config.validator_settings.validator_cookie_path.is_none());
    }

    #[test]
    fn test_public_ip_still_rejected() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
backend = "fetch"
network = "Testnet"

[validator_settings]
validator_jsonrpc_listen_address = "8.8.8.8:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "public_ip.toml");
        let result = load_config(&config_path);
        assert!(result.is_err());

        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.contains("private IP"));
        }
    }

    #[test]
    fn test_sensitive_env_var_blocked() {
        let guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        guard.set_var("ZAINO_VALIDATOR_SETTINGS__VALIDATOR_PASSWORD", "secret123");

        let config_path =
            create_test_config_file(&temp_dir, toml_content, "sensitive_env_test.toml");
        let result = load_config(&config_path);
        assert!(result.is_err());

        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.contains("sensitive key"));
            assert!(msg.contains("VALIDATOR_PASSWORD"));
        }
    }

    #[test]
    fn test_sensitive_key_detection() {
        assert!(is_sensitive_leaf_key("password"));
        assert!(is_sensitive_leaf_key("PASSWORD"));
        assert!(is_sensitive_leaf_key("validator_password"));
        assert!(is_sensitive_leaf_key("VALIDATOR_PASSWORD"));
        assert!(is_sensitive_leaf_key("secret"));
        assert!(is_sensitive_leaf_key("api_token"));
        assert!(is_sensitive_leaf_key("cookie"));
        assert!(is_sensitive_leaf_key("private_key"));

        assert!(!is_sensitive_leaf_key("username"));
        assert!(!is_sensitive_leaf_key("address"));
        assert!(!is_sensitive_leaf_key("network"));
    }

    #[test]
    fn test_unknown_fields_rejected() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
unknown_field = "value"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path = create_test_config_file(&temp_dir, toml_content, "unknown_fields.toml");
        let result = load_config(&config_path);
        assert!(result.is_err());
    }

    /// Verifies that `generate_default_config()` produces valid TOML.
    ///
    /// TOML requires simple values before table sections. If ZainodConfig field
    /// order changes incorrectly, serialization fails with "values must be
    /// emitted before tables". This test catches that regression.
    #[test]
    fn test_generate_default_config_produces_valid_toml() {
        let content = generate_default_config().expect("should generate config");
        assert!(content.starts_with(GENERATED_CONFIG_HEADER));

        let toml_part = content.strip_prefix(GENERATED_CONFIG_HEADER).unwrap();
        let parsed: Result<toml::Value, _> = toml::from_str(toml_part);
        assert!(
            parsed.is_ok(),
            "Generated config is not valid TOML: {:?}",
            parsed.err()
        );
    }

    /// Verifies config survives serialize → deserialize → serialize roundtrip.
    ///
    /// Catches regressions in custom serde impls (DatabaseSize, Network) and
    /// ensures field ordering remains stable. If the second serialization differs
    /// from the first, something is being lost or transformed during the roundtrip.
    #[test]
    fn test_config_roundtrip_serialize_deserialize() {
        let original = ZainodConfig::default();

        let toml_str = toml::to_string_pretty(&original).expect("should serialize");
        let roundtripped: ZainodConfig = toml::from_str(&toml_str).expect("should deserialize");
        let toml_str_again = toml::to_string_pretty(&roundtripped).expect("should serialize again");

        assert_eq!(
            toml_str, toml_str_again,
            "config roundtrip should be stable"
        );
    }

    // --- donation_address ---

    #[test]
    fn donation_address_valid_is_accepted() {
        use zcash_address::{ToAddress as _, ZcashAddress};
        use zcash_protocol::consensus::NetworkType;

        let _guard = EnvGuard::new();
        let dir = TempDir::new().unwrap();

        let valid_addr =
            ZcashAddress::from_transparent_p2pkh(NetworkType::Main, [1u8; 20]).encode();

        let content = format!(
            "donation_address = {:?}\n\
             [grpc_settings]\n\
             listen_address = \"127.0.0.1:8232\"\n",
            valid_addr,
        );
        let path = create_test_config_file(&dir, &content, "valid_donation.toml");
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.donation_address.unwrap().to_string(), valid_addr);
    }

    #[test]
    fn donation_address_invalid_is_rejected() {
        let _guard = EnvGuard::new();
        let dir = TempDir::new().unwrap();

        let content = "donation_address = \"not_a_zcash_address\"\n\
             [grpc_settings]\n\
             listen_address = \"127.0.0.1:8232\"\n";
        let path = create_test_config_file(&dir, content, "invalid_donation.toml");
        assert!(load_config(&path).is_err());
    }

    /// `LightdInfo.version` (issue #1057) is sourced from
    /// `*ServiceConfig.indexer_version`. This must be set to `zainod`'s
    /// `CARGO_PKG_VERSION` at the boundary so the wire reflects the
    /// deployed binary, not zaino-state's library version.
    #[test]
    #[allow(deprecated)]
    fn indexer_version_is_zainod_pkg_version() {
        let _guard = EnvGuard::new();

        let cfg = ZainodConfig::default();

        let state_cfg = StateServiceConfig::try_from(cfg.clone())
            .expect("StateServiceConfig conversion should succeed for default ZainodConfig");
        assert_eq!(state_cfg.common.indexer_version, env!("CARGO_PKG_VERSION"));

        let fetch_cfg = FetchServiceConfig::try_from(cfg)
            .expect("FetchServiceConfig conversion should succeed for default ZainodConfig");
        assert_eq!(fetch_cfg.common.indexer_version, env!("CARGO_PKG_VERSION"));
    }

    /// `StateServiceConfig::try_from` and `FetchServiceConfig::try_from`
    /// share a single `build_common` helper, so the two backends can
    /// never quietly disagree on the common payload they hand to a
    /// service. Locks that property in across every field: a future
    /// hand-rolled divergence (e.g. one path stops applying the
    /// missing-credentials sentinel, or a new common field gets
    /// populated on only one side) makes this fail. Pretty-Debug
    /// equality is used because not every constituent of
    /// `CommonBackendConfig` derives `PartialEq`, and a single
    /// stringified compare future-proofs the test against fields added
    /// later.
    #[test]
    #[allow(deprecated)]
    fn state_and_fetch_common_payloads_agree() {
        let _guard = EnvGuard::new();

        let cfg = ZainodConfig::default();

        let state_config = StateServiceConfig::try_from(cfg.clone())
            .expect("StateServiceConfig conversion should succeed for default ZainodConfig");
        let fetch_config = FetchServiceConfig::try_from(cfg)
            .expect("FetchServiceConfig conversion should succeed for default ZainodConfig");

        assert_eq!(
            format!("{:#?}", state_config.common),
            format!("{:#?}", fetch_config.common),
        );

        let cfg = ZainodConfig {
            ephemeral_finalised_state: true,
            ..ZainodConfig::default()
        };

        let state_config = StateServiceConfig::try_from(cfg.clone())
            .expect("StateServiceConfig conversion should succeed for ephemeral finalised state");
        let fetch_config = FetchServiceConfig::try_from(cfg)
            .expect("FetchServiceConfig conversion should succeed for ephemeral finalised state");

        assert!(state_config.common.ephemeral_finalised_state);
        assert!(fetch_config.common.ephemeral_finalised_state);

        assert_eq!(
            format!("{:#?}", state_config.common),
            format!("{:#?}", fetch_config.common),
        );
    }

    /// Builds a default config with the JSON-RPC server bound to `addr`.
    ///
    /// The default config otherwise passes `check_config` (loopback gRPC,
    /// private validator), so the JSON-RPC bind address is isolated as the only
    /// variable under test. A non-default port avoids the gRPC/JSON-RPC
    /// same-address check.
    fn json_config_with(addr: &str) -> ZainodConfig {
        ZainodConfig {
            json_server_settings: Some(JsonRpcServerConfig {
                json_rpc_listen_address: addr.parse().expect("test bind address must parse"),
                cookie_dir: None,
            }),
            ..ZainodConfig::default()
        }
    }

    #[test]
    fn json_rpc_loopback_bind_is_accepted() {
        json_config_with("127.0.0.1:8237")
            .check_config()
            .expect("loopback JSON-RPC bind must be accepted");
    }

    #[test]
    fn json_rpc_private_ipv4_bind_is_accepted() {
        json_config_with("192.168.1.10:8237")
            .check_config()
            .expect("RFC1918 JSON-RPC bind must be accepted");
    }

    #[test]
    fn json_rpc_ipv6_ula_bind_is_accepted() {
        json_config_with("[fc00::1]:8237")
            .check_config()
            .expect("IPv6 ULA JSON-RPC bind must be accepted");
    }

    #[test]
    fn no_json_server_settings_is_accepted() {
        let cfg = ZainodConfig {
            json_server_settings: None,
            ..ZainodConfig::default()
        };
        cfg.check_config()
            .expect("config without a JSON-RPC server must be accepted");
    }

    // The rejection rule is compiled out when the override feature is enabled,
    // so these tests only apply to the default build.
    #[cfg(not(feature = "allow_unencrypted_public_json_rpc_bind"))]
    #[test]
    fn json_rpc_public_bind_is_rejected() {
        match json_config_with("8.8.8.8:8237").check_config() {
            Err(IndexerError::ConfigError(msg)) => assert!(
                msg.contains("allow_unencrypted_public_json_rpc_bind"),
                "error should name the override feature, got: {msg}"
            ),
            other => panic!("expected ConfigError for public JSON-RPC bind, got {other:?}"),
        }
    }

    #[cfg(not(feature = "allow_unencrypted_public_json_rpc_bind"))]
    #[test]
    fn json_rpc_unspecified_bind_is_rejected() {
        // 0.0.0.0 binds all interfaces (including public) and is not private.
        match json_config_with("0.0.0.0:8237").check_config() {
            Err(IndexerError::ConfigError(_)) => {}
            other => panic!("expected ConfigError for unspecified JSON-RPC bind, got {other:?}"),
        }
    }

    #[cfg(feature = "allow_unencrypted_public_json_rpc_bind")]
    #[test]
    fn json_rpc_public_bind_allowed_with_feature() {
        json_config_with("8.8.8.8:8237")
            .check_config()
            .expect("public JSON-RPC bind must be accepted under the override feature");
    }

    #[test]
    fn test_ephemeral_finalised_state_config_is_deserialized() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();

        let toml_content = r#"
backend = "fetch"
network = "Testnet"
ephemeral_finalised_state = true

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:18232"

[storage.database]
path = "/zaino/db"

[grpc_settings]
listen_address = "127.0.0.1:8137"
"#;

        let config_path =
            create_test_config_file(&temp_dir, toml_content, "ephemeral_finalised_state.toml");
        let config = load_config(&config_path).expect("load_config failed");

        assert!(config.ephemeral_finalised_state);

        #[allow(deprecated)]
        {
            let fetch_config = FetchServiceConfig::try_from(config)
                .expect("FetchServiceConfig conversion should succeed");

            assert!(fetch_config.common.ephemeral_finalised_state);
        }
    }
}
