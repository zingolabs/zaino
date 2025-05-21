//! Zaino config.

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

// Added for Serde deserialization helpers
use serde::{
    de::{self, Deserializer},
    Deserialize,
};
use toml; // Ensure toml crate is available for from_str
use tracing::{info, warn};
use zaino_state::{BackendConfig, BackendType, FetchServiceConfig, StateServiceConfig};

use crate::error::IndexerError;

/// Custom deserialization function for `SocketAddr` from a String.
/// Used by Serde's `deserialize_with`.
fn deserialize_socketaddr_from_string<'de, D>(deserializer: D) -> Result<SocketAddr, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    fetch_socket_addr_from_hostname(&s)
        .map_err(|e| de::Error::custom(format!("Invalid socket address string '{}': {}", s, e)))
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
            "Invalid backend type '{}', valid options are 'state' or 'fetch'",
            s
        ))),
    }
}

/// Config information required for Zaino.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct IndexerConfig {
    /// Type of backend to be used.
    #[serde(deserialize_with = "deserialize_backendtype_from_string")]
    pub backend: BackendType,
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
                        "TLS is enabled, but certificate path '{}' does not exist.",
                        cert_path
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
                        "TLS is enabled, but key path '{}' does not exist.",
                        key_path
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
                        format!("Validator cookie authentication is enabled, but cookie path '{}' does not exist.", cookie_path),
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
            // TODO: change to BackendType::State.
            backend: BackendType::Fetch,
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
fn default_zaino_db_path() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".cache").join("zaino"),
        Err(_) => PathBuf::from("/tmp").join("zaino").join(".cache"),
    }
}

/// Loads the default file path for zebras's local db.
fn default_zebra_db_path() -> Result<PathBuf, IndexerError> {
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
            .map_err(|e| {
                IndexerError::ConfigError(format!("Invalid address '{}': {}", address, e))
            })?
            .collect();
        if let Some(ipv4_addr) = addrs.iter().find(|addr| addr.is_ipv4()) {
            Ok(*ipv4_addr)
        } else {
            addrs.into_iter().next().ok_or_else(|| {
                IndexerError::ConfigError(format!("Unable to resolve address '{}'", address))
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
pub fn load_config(file_path: &std::path::PathBuf) -> Result<IndexerConfig, IndexerError> {
    match std::fs::read_to_string(file_path) {
        Ok(contents) => {
            match toml::from_str::<IndexerConfig>(&contents) {
                Ok(parsed_config) => {
                    let finalized_config = parsed_config.finalize_config_logic();
                    finalized_config.check_config()?;
                    info!(
                        "Successfully loaded and validated config from '{}'",
                        file_path.display()
                    );
                    Ok(finalized_config)
                }
                Err(e) => {
                    warn!(
                        "Failed to parse TOML from '{}': {}. Using default configuration.",
                        file_path.display(),
                        e
                    );
                    let finalized_default = IndexerConfig::default().finalize_config_logic();
                    // It's good practice to ensure the default config is also valid.
                    finalized_default.check_config().map_err(|check_err| {
                        IndexerError::ConfigError(format!(
                            "Default configuration is invalid: {}",
                            check_err
                        ))
                    })?;
                    Ok(finalized_default)
                }
            }
        }
        Err(e) => {
            warn!(
                "Could not read config file at '{}': {}. Using default configuration.",
                file_path.display(),
                e
            );
            let finalized_default = IndexerConfig::default().finalize_config_logic();
            finalized_default.check_config().map_err(|check_err| {
                IndexerError::ConfigError(format!(
                    "Default configuration is invalid: {}",
                    check_err
                ))
            })?;
            Ok(finalized_default)
        }
    }
}

impl TryFrom<IndexerConfig> for BackendConfig {
    type Error = IndexerError;

    fn try_from(cfg: IndexerConfig) -> Result<Self, Self::Error> {
        let network = cfg.get_network()?;

        match cfg.backend {
            BackendType::State => Ok(BackendConfig::State(StateServiceConfig {
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

            BackendType::Fetch => Ok(BackendConfig::Fetch(FetchServiceConfig {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full_valid_config() {
        let toml_str = r#"
            backend = "fetch"
            enable_json_server = true
            json_rpc_listen_address = "127.0.0.1:8000"
            enable_cookie_auth = true
            cookie_dir = "/tmp/zaino-cookie"
            grpc_listen_address = "0.0.0.0:9000"
            grpc_tls = true
            tls_cert_path = "/path/to/cert.pem"
            tls_key_path = "/path/to/key.pem"
            validator_listen_address = "192.168.1.10:18232"
            validator_cookie_auth = true
            validator_cookie_path = "/var/run/zec/.cookie"
            validator_user = "user"
            validator_password = "password"
            map_capacity = 10000
            map_shard_amount = 16
            zaino_db_path = "/db/zaino"
            zebra_db_path = "/db/zebra"
            db_size = 100
            network = "Mainnet"
            no_sync = false
            no_db = false
            slow_sync = false
        "#;
        let config: IndexerConfig =
            toml::from_str(toml_str).expect("Failed to parse full valid config");
        let finalized_config = config.finalize_config_logic();

        assert_eq!(finalized_config.backend, BackendType::Fetch);
        assert_eq!(finalized_config.enable_json_server, true);
        assert_eq!(
            finalized_config.json_rpc_listen_address,
            "127.0.0.1:8000".parse().unwrap()
        );
        assert_eq!(finalized_config.enable_cookie_auth, true);
        assert_eq!(
            finalized_config.cookie_dir,
            Some(PathBuf::from("/tmp/zaino-cookie"))
        );
        assert_eq!(
            finalized_config.grpc_listen_address,
            "0.0.0.0:9000".parse().unwrap()
        );
        assert_eq!(finalized_config.grpc_tls, true);
        assert_eq!(
            finalized_config.tls_cert_path,
            Some("/path/to/cert.pem".to_string())
        );
        assert_eq!(
            finalized_config.tls_key_path,
            Some("/path/to/key.pem".to_string())
        );
        assert_eq!(
            finalized_config.validator_listen_address,
            "192.168.1.10:18232".parse().unwrap()
        );
        assert_eq!(finalized_config.validator_cookie_auth, true);
        assert_eq!(
            finalized_config.validator_cookie_path,
            Some("/var/run/zec/.cookie".to_string())
        );
        assert_eq!(finalized_config.validator_user, Some("user".to_string()));
        assert_eq!(
            finalized_config.validator_password,
            Some("password".to_string())
        );
        assert_eq!(finalized_config.map_capacity, Some(10000));
        assert_eq!(finalized_config.map_shard_amount, Some(16));
        assert_eq!(finalized_config.zaino_db_path, PathBuf::from("/db/zaino"));
        assert_eq!(finalized_config.zebra_db_path, PathBuf::from("/db/zebra"));
        assert_eq!(finalized_config.db_size, Some(100));
        assert_eq!(finalized_config.network, "Mainnet");
        assert_eq!(finalized_config.no_sync, false);
        assert_eq!(finalized_config.no_db, false);
        assert_eq!(finalized_config.slow_sync, false);

        let check_result = finalized_config.check_config();
        if finalized_config.grpc_tls
            && (finalized_config.tls_cert_path.is_some() || finalized_config.tls_key_path.is_some())
        {
            // If TLS is on and paths are specified, we expect it to fail due to non-existent paths in test env.
            // Or if paths are None, it should also fail.
            assert!(
                check_result.is_err(),
                "check_config should fail if TLS is on and paths are not valid/existent or missing"
            );
            if let Err(e) = &check_result {
                let msg = e.to_string();
                // It could fail because paths are None, or because paths are Some but don't exist.
                assert!(
                    msg.contains("does not exist")
                        || msg.contains("no certificate path is provided")
                        || msg.contains("no key path is provided"),
                    "Error message should be about non-existent or missing TLS paths: {}",
                    msg
                );
            }
        } else if finalized_config.validator_cookie_auth
            && finalized_config.validator_cookie_path.is_none()
        {
            // If validator cookie auth is on and path is None, it should fail.
            assert!(
                check_result.is_err(),
                "check_config should fail if validator_cookie_auth is on and path is None"
            );
            if let Err(e) = &check_result {
                let msg = e.to_string();
                assert!(
                    msg.contains("no cookie path is provided"),
                    "Error message should be about missing validator cookie path: {}",
                    msg
                );
            }
        } else if check_result.is_err() {
            // If it failed for other reasons not covered by specific test conditions above (like default DB paths not existing)
            // we check if it's a path existence error, which is acceptable in unit test context for default paths.
            let msg = check_result.as_ref().err().unwrap().to_string();
            assert!(
                msg.contains("does not exist"),
                "Unexpected error in check_config for full_valid_config: {}",
                msg
            );
        }
        // If none of the above error conditions specific to path existence for configured features were met,
        // and it didn't error for other path reasons, it implies other checks passed.
    }

    #[test]
    fn test_deserialize_optional_fields_missing() {
        let toml_str = r#"
            backend = "state"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/opt/zaino/data"
            zebra_db_path = "/opt/zebra/data"
            network = "Testnet"
        "#;
        let config: IndexerConfig =
            toml::from_str(toml_str).expect("Failed to parse minimal config");
        let finalized_config = config.finalize_config_logic();
        let default_values = IndexerConfig::default();

        assert_eq!(finalized_config.backend, BackendType::State);
        assert_eq!(
            finalized_config.enable_json_server,
            default_values.enable_json_server
        );
        assert_eq!(
            finalized_config.enable_cookie_auth,
            default_values.enable_cookie_auth
        );
        assert_eq!(finalized_config.cookie_dir, None); // Default is None, and enable_cookie_auth is false by default
        assert_eq!(finalized_config.grpc_tls, default_values.grpc_tls);
        assert_eq!(finalized_config.tls_cert_path, None);
        assert_eq!(finalized_config.tls_key_path, None);
        assert_eq!(
            finalized_config.validator_cookie_auth,
            default_values.validator_cookie_auth
        );
        assert_eq!(finalized_config.validator_cookie_path, None);
        assert_eq!(
            finalized_config.validator_user,
            default_values.validator_user
        );
        assert_eq!(
            finalized_config.validator_password,
            default_values.validator_password
        );
        assert_eq!(finalized_config.map_capacity, None);
        assert_eq!(finalized_config.map_shard_amount, None);
        assert_eq!(
            finalized_config.zaino_db_path,
            PathBuf::from("/opt/zaino/data")
        );
        assert_eq!(
            finalized_config.zebra_db_path,
            PathBuf::from("/opt/zebra/data")
        );
        assert_eq!(finalized_config.db_size, None);
        assert_eq!(finalized_config.network, "Testnet");

        // With default grpc_tls=false and validator_cookie_auth=false, path checks for these are skipped.
        // However, check_config might still fail if default zaino_db_path/zebra_db_path don't exist.
        // If the provided paths are used, it should pass those specific checks.
        // Let's assume for this test, if it fails, it might be due to other path checks or network name not being Mainnet/Testnet/Regtest (which is Testnet, so ok).
        match finalized_config.check_config() {
            Ok(_) => {}
            Err(e) => {
                // It's acceptable for it to fail here if the default_zaino_db_path etc. from default() don't exist.
                // The crucial part for *this test* is that Serde correctly parsed missing optional fields to None.
                // And that the provided /opt/zaino/data was used.
                if !e.to_string().contains("does not exist") {
                    panic!(
                        "check_config failed for unexpected reason in optional_fields_missing: {}",
                        e
                    );
                }
            }
        }

        // This part of the test remains important to show how check_config *would* fail if flags were different
        let mut config_with_tls_issue = finalized_config.clone();
        config_with_tls_issue.grpc_tls = true;
        assert!(
            config_with_tls_issue.check_config().is_err(),
            "check_config should fail if grpc_tls is true and cert paths are None"
        );
    }

    #[test]
    fn test_cookie_dir_logic() {
        // Scenario 1: auth enabled, cookie_dir missing
        let toml_s1 = r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = true
        "#;
        let config1: IndexerConfig = toml::from_str(toml_s1).unwrap();
        let finalized_config1 = config1.finalize_config_logic();
        assert_eq!(
            finalized_config1.cookie_dir,
            Some(default_ephemeral_cookie_path())
        );

        // Scenario 2: auth enabled, cookie_dir specified
        let toml_s2 = r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = true
            cookie_dir = "/my/cookie/path"
        "#;
        let config2: IndexerConfig = toml::from_str(toml_s2).unwrap();
        let finalized_config2 = config2.finalize_config_logic();
        assert_eq!(
            finalized_config2.cookie_dir,
            Some(PathBuf::from("/my/cookie/path"))
        );

        // Scenario 3: auth disabled, cookie_dir specified (should be None after finalize)
        let toml_s3 = r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = false
            cookie_dir = "/my/ignored/path"
        "#;
        let config3: IndexerConfig = toml::from_str(toml_s3).unwrap();
        let finalized_config3 = config3.finalize_config_logic();
        assert_eq!(finalized_config3.cookie_dir, None);

        // Scenario 4: auth disabled, cookie_dir missing (should remain None)
        let toml_s4 = r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = false
        "#;
        let config4: IndexerConfig = toml::from_str(toml_s4).unwrap();
        let finalized_config4 = config4.finalize_config_logic();
        assert_eq!(finalized_config4.cookie_dir, None);
    }

    #[test]
    fn test_string_none_as_path_for_cookie_dir() {
        let toml_str_auth_enabled = r#"
            backend = "fetch"
            enable_cookie_auth = true
            cookie_dir = "None"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#;
        let config_auth_enabled: IndexerConfig = toml::from_str(toml_str_auth_enabled).unwrap();
        let finalized_config_auth_enabled = config_auth_enabled.finalize_config_logic();
        // Now, "None" is a literal path if auth is enabled and cookie_dir was explicitly set to this string.
        assert_eq!(
            finalized_config_auth_enabled.cookie_dir,
            Some(PathBuf::from("None"))
        );

        let toml_str_auth_disabled = r#"
            backend = "fetch"
            enable_cookie_auth = false
            cookie_dir = "None"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#;
        let config_auth_disabled: IndexerConfig = toml::from_str(toml_str_auth_disabled).unwrap();
        let finalized_config_auth_disabled = config_auth_disabled.finalize_config_logic();
        // If auth is disabled, cookie_dir becomes None, regardless of what it was.
        assert_eq!(finalized_config_auth_disabled.cookie_dir, None);
    }

    #[test]
    fn test_deserialize_empty_string_yields_default() {
        let toml_str = "";
        let parsed_config: IndexerConfig = toml::from_str(toml_str)
            .expect("Parsing empty string should yield default config based on #[serde(default)]");
        let finalized_config = parsed_config.finalize_config_logic();

        let default_config = IndexerConfig::default().finalize_config_logic();

        assert_eq!(finalized_config.backend, default_config.backend);
        assert_eq!(
            finalized_config.enable_json_server,
            default_config.enable_json_server
        );
        assert_eq!(
            finalized_config.json_rpc_listen_address,
            default_config.json_rpc_listen_address
        );
        assert_eq!(
            finalized_config.enable_cookie_auth,
            default_config.enable_cookie_auth
        );
        assert_eq!(finalized_config.cookie_dir, default_config.cookie_dir); // finalized_default.cookie_dir will be None if enable_cookie_auth is false by default
        assert_eq!(
            finalized_config.grpc_listen_address,
            default_config.grpc_listen_address
        );
        assert_eq!(finalized_config.grpc_tls, default_config.grpc_tls);
        assert_eq!(finalized_config.tls_cert_path, default_config.tls_cert_path);
        assert_eq!(finalized_config.tls_key_path, default_config.tls_key_path);
        assert_eq!(
            finalized_config.validator_listen_address,
            default_config.validator_listen_address
        );
        assert_eq!(
            finalized_config.validator_cookie_auth,
            default_config.validator_cookie_auth
        );
        assert_eq!(
            finalized_config.validator_cookie_path,
            default_config.validator_cookie_path
        );
        assert_eq!(
            finalized_config.validator_user,
            default_config.validator_user
        );
        assert_eq!(
            finalized_config.validator_password,
            default_config.validator_password
        );
        assert_eq!(finalized_config.map_capacity, default_config.map_capacity);
        assert_eq!(
            finalized_config.map_shard_amount,
            default_config.map_shard_amount
        );
        assert_eq!(finalized_config.zaino_db_path, default_config.zaino_db_path);
        assert_eq!(finalized_config.zebra_db_path, default_config.zebra_db_path);
        assert_eq!(finalized_config.db_size, default_config.db_size);
        assert_eq!(finalized_config.network, default_config.network);
        assert_eq!(finalized_config.no_sync, default_config.no_sync);
        assert_eq!(finalized_config.no_db, default_config.no_db);
        assert_eq!(finalized_config.slow_sync, default_config.slow_sync);
        // The default config itself should be valid, assuming default paths exist or checks are lenient for defaults.
        // This might require mocks or specific test environment if default paths must exist.
        finalized_config.check_config().expect("Default config after finalization should be valid, or checks need adjustment for default scenario");
    }

    #[test]
    fn test_deserialize_invalid_backend_type() {
        let toml_str = r#"backend = "invalid_type""#;
        assert!(toml::from_str::<IndexerConfig>(toml_str).is_err());
    }

    #[test]
    fn test_deserialize_invalid_socket_address() {
        let toml_str = r#"json_rpc_listen_address = "not-a-valid-address""#;
        assert!(toml::from_str::<IndexerConfig>(toml_str).is_err());

        let toml_str_port_too_high = r#"json_rpc_listen_address = "127.0.0.1:70000""#;
        assert!(toml::from_str::<IndexerConfig>(toml_str_port_too_high).is_err());
    }

    #[test]
    fn test_parse_existing_zindexer_toml_content() {
        // Include the actual zindexer.toml file content at compile time
        let zindexer_toml_content = include_str!("../zindexer.toml");

        // Assert that parsing the original zindexer.toml content fails
        // due to string "None" for Option<usize> fields (and similar issues).
        assert!(
            toml::from_str::<IndexerConfig>(zindexer_toml_content).is_err(),
            "Parsing the actual zindexer.toml (with string 'None' for Option<usize> etc.) should fail."
        );

        // Test with a version of zindexer.toml that IS expected to parse successfully
        // by omitting or correcting the problematic "None" string for numeric/boolean Optionals.
        let zindexer_toml_adjusted_for_direct_parse = r#"
            backend = "fetch"
            enable_json_server =  false
            json_rpc_listen_address = "127.0.0.1:8237"
            enable_cookie_auth = false
            # cookie_dir = "None" // Omitted, will be None, then handled by finalize_config_logic
            grpc_listen_address = "127.0.0.1:8137"
            grpc_tls = false
            # tls_cert_path = "None" // Omitted, becomes None
            # tls_key_path = "None"  // Omitted, becomes None
            validator_listen_address = "127.0.0.1:18232"
            validator_cookie_auth = false
            # validator_cookie_path = "None" // Omitted, becomes None
            validator_user = "xxxxxx"
            validator_password = "xxxxxx"
            # map_capacity omitted
            # map_shard_amount omitted
            zaino_db_path = "/path/to/zaino_db_explicit" # Explicit valid path
            zebra_db_path = "/path/to/zebra_db_explicit" # Explicit valid path
            # db_size omitted
            network = "Testnet"
            no_sync = false
            no_db = false
            slow_sync = false
        "#;

        let config: IndexerConfig = toml::from_str(zindexer_toml_adjusted_for_direct_parse)
            .expect("Failed to parse adjusted zindexer.toml content for successful parse test");
        let finalized_config = config.finalize_config_logic();

        assert_eq!(finalized_config.backend, BackendType::Fetch);
        assert_eq!(finalized_config.enable_json_server, false);
        assert_eq!(
            finalized_config.json_rpc_listen_address,
            "127.0.0.1:8237".parse().unwrap()
        );
        assert_eq!(finalized_config.enable_cookie_auth, false);
        assert_eq!(finalized_config.cookie_dir, None);
        assert_eq!(finalized_config.tls_cert_path, None);
        assert_eq!(finalized_config.tls_key_path, None);
        assert_eq!(finalized_config.validator_cookie_path, None);
        assert_eq!(finalized_config.map_capacity, None);
        assert_eq!(
            finalized_config.zaino_db_path,
            PathBuf::from("/path/to/zaino_db_explicit")
        );
        assert_eq!(
            finalized_config.zebra_db_path,
            PathBuf::from("/path/to/zebra_db_explicit")
        );

        // The full validity according to check_config depends on existence of specified paths if flags are true.
        // For this test, the main point is that Serde parsing worked for the adjusted structure.
        // We expect check_config to fail if, for example, grpc_tls were true and paths were missing.
        // Since they are false and paths are None, some checks might pass, but not all required for a fully operational config.
        // A more robust check_config test would mock file system operations.
        if finalized_config.grpc_tls
            && (finalized_config.tls_cert_path.is_none() || finalized_config.tls_key_path.is_none())
        {
            assert!(finalized_config.check_config().is_err());
        } else if finalized_config.validator_cookie_auth
            && finalized_config.validator_cookie_path.is_none()
        {
            assert!(finalized_config.check_config().is_err());
        } else {
            // If the above specific error conditions for TLS/validator cookie paths aren't met
            // (because grpc_tls and validator_cookie_auth are false in zindexer_toml_adjusted_for_direct_parse),
            // then check_config should pass, assuming other checks (network name, IP validity) are satisfied.
            // The explicit paths for zaino_db_path and zebra_db_path are not currently checked for existence by check_config.
            finalized_config.check_config().expect("check_config should pass for the adjusted TOML with explicit non-TLS/non-validator-cookie paths");
        }
    }
}
