//! Zaino config.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use tracing::info;
#[cfg(feature = "no_tls_use_unencrypted_traffic")]
use tracing::warn;

use crate::error::IndexerError;
use zaino_common::{
    try_resolve_address, AddressResolution, CacheConfig, DatabaseConfig, DatabaseSize, Network,
    ServiceConfig, StorageConfig, ValidatorConfig,
};
use zaino_serve::server::config::{GrpcServerConfig, JsonRpcServerConfig};
#[allow(deprecated)]
use zaino_state::{BackendType, FetchServiceConfig, StateServiceConfig};

/// Sensitive key suffixes that should not be set via environment variables.
const SENSITIVE_KEY_SUFFIXES: [&str; 5] = ["password", "secret", "token", "cookie", "private_key"];

/// Checks if a key is sensitive and should not be set via environment variables.
fn is_sensitive_leaf_key(leaf_key: &str) -> bool {
    let key = leaf_key.to_ascii_lowercase();
    SENSITIVE_KEY_SUFFIXES
        .iter()
        .any(|suffix| key.ends_with(suffix))
}

/// Config information required for Zaino.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ZainodConfig {
    /// Type of backend to be used.
    pub backend: BackendType,
    /// Enable JsonRPC server with a valid Some value.
    pub json_server_settings: Option<JsonRpcServerConfig>,
    /// gRPC server settings including listen addr, tls status, key and cert.
    pub grpc_settings: GrpcServerConfig,
    /// Full node / validator configuration settings.
    pub validator_settings: ValidatorConfig,
    /// Service-level configuration (timeout, channel size).
    pub service: ServiceConfig,
    /// Storage configuration (cache and database).
    pub storage: StorageConfig,
    /// Block Cache database file path (ZebraDB location).
    pub zebra_db_path: PathBuf,
    /// Network chain type.
    pub network: Network,
}

impl ZainodConfig {
    /// Performs checks on config data.
    pub(crate) fn check_config(&self) -> Result<(), IndexerError> {
        // Check TLS settings.
        if self.grpc_settings.tls.is_some() {
            let tls = self.grpc_settings.tls.as_ref().expect("to be Some");

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

        Ok(StateServiceConfig {
            validator_state_config: zebra_state::Config {
                cache_dir: cfg.zebra_db_path.clone(),
                ephemeral: false,
                delete_old_database: true,
                debug_stop_at_height: None,
                debug_validity_check_interval: None,
                should_backup_non_finalized_state: true,
            },
            validator_rpc_address: cfg
                .validator_settings
                .validator_jsonrpc_listen_address
                .clone(),
            validator_grpc_address,
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
        })
    }
}

#[allow(deprecated)]
impl TryFrom<ZainodConfig> for FetchServiceConfig {
    type Error = IndexerError;

    fn try_from(cfg: ZainodConfig) -> Result<Self, Self::Error> {
        Ok(FetchServiceConfig {
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
        })
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
    fn test_parse_zindexer_toml_integration() {
        let _guard = EnvGuard::new();
        let temp_dir = TempDir::new().unwrap();
        let zindexer_toml_content = include_str!("../zindexer.toml");

        let config_path =
            create_test_config_file(&temp_dir, zindexer_toml_content, "zindexer_test.toml");
        let config = load_config(&config_path).expect("load_config failed to parse zindexer.toml");
        let defaults = ZainodConfig::default();

        assert_eq!(config.backend, BackendType::Fetch);
        assert_eq!(
            config.validator_settings.validator_user,
            defaults.validator_settings.validator_user
        );
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

        let config_path = create_test_config_file(&temp_dir, toml_content, "test_config.toml");
        let config = load_config(&config_path).expect("load_config should succeed");

        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.storage.cache.capacity, 12345);
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
}
