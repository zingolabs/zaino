//! Zaino config.

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    str::FromStr,
};

use toml::Value;
use tracing::warn;

use crate::error::IndexerError;

/// Arguments:
/// config: the config to parse
/// field: the name of the variable to define, which matches the name of
///     the config field
/// kind: the toml::Value variant to parse the config field as
/// default: the default config to fall back on
/// handle: a function which returns an Option. Usually, this is
///     simply [Some], to wrap the parsed value in Some when found
macro_rules! parse_field_or_warn_and_default {
    ($config:ident, $field:ident, $kind:ident, $default:ident, $handle:expr) => {
        let $field = $config
            .get(stringify!($field))
            .map(|value| match value {
                Value::String(string) if string == "None" => None,
                Value::$kind(val_inner) => $handle(val_inner.clone()),
                _ => {
                    warn!("Invalid `{}`, using default.", stringify!($field));
                    None
                }
            })
            .unwrap_or_else(|| {
                warn!("Missing `{}`, using default.", stringify!($field));
                None
            })
            .unwrap_or_else(|| $default.$field);
    };
}

/// Config information required for Zaino.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct IndexerConfig {
    /// gRPC server bind addr.
    pub grpc_listen_address: SocketAddr,
    /// Enables TLS.
    pub grpc_tls: bool,
    /// Path to the TLS certificate file.
    pub tls_cert_path: Option<String>,
    /// Path to the TLS private key file.
    pub tls_key_path: Option<String>,
    /// Full node / validator listen port.
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
    /// This is Zaino's Compact Block Cache db if using the FetchService or Zebra's RocksDB if using the StateService.
    pub db_path: PathBuf,
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
    /// Disables internal mempool and blockcache.
    ///
    /// For use by lightweight wallets that do not want to run any extra processes.
    ///
    /// NOTE: Currently unimplemented as will require either a Tonic backend or a JsonRPC server.
    pub no_state: bool,
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

        #[cfg(not(feature = "test_only_very_insecure"))]
        let grpc_addr = fetch_socket_addr_from_hostname(&self.grpc_listen_address.to_string())?;
        #[cfg(feature = "test_only_very_insecure")]
        let _ = fetch_socket_addr_from_hostname(&self.grpc_listen_address.to_string())?;

        let validator_addr =
            fetch_socket_addr_from_hostname(&self.validator_listen_address.to_string())?;

        // Ensure validator listen address is private.
        if !is_private_listen_addr(&validator_addr) {
            return Err(IndexerError::ConfigError(
                "Zaino may only connect to Zebra with private IP addresses.".to_string(),
            ));
        }

        #[cfg(not(feature = "test_only_very_insecure"))]
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
        #[cfg(feature = "test_only_very_insecure")]
        {
            warn!("Zaino built using test_only_very_insecure feature, proceed with caution.");
        }

        Ok(())
    }

    /// Returns the network type currently being used by the server.
    pub fn get_network(&self) -> Result<zebra_chain::parameters::Network, IndexerError> {
        match self.network.as_str() {
            "Regtest" => Ok(zebra_chain::parameters::Network::new_regtest(
                Some(1),
                Some(1),
            )),
            "Testnet" => Ok(zebra_chain::parameters::Network::new_default_testnet()),
            "Mainnet" => Ok(zebra_chain::parameters::Network::Mainnet),
            _ => Err(IndexerError::ConfigError(
                "Incorrect network name given.".to_string(),
            )),
        }
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
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
            db_path: default_db_path(),
            db_size: None,
            network: "Testnet".to_string(),
            no_sync: false,
            no_db: false,
            no_state: false,
        }
    }
}

/// Loads the default file path for zaino's local db.
fn default_db_path() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".cache").join("zaino"),
        Err(_) => PathBuf::from("/tmp").join("zaino"),
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
#[cfg_attr(feature = "test_only_very_insecure", allow(dead_code))]
pub(crate) fn is_loopback_listen_addr(addr: &SocketAddr) -> bool {
    let ip = addr.ip();
    match ip {
        IpAddr::V4(ipv4) => ipv4.is_loopback(),
        IpAddr::V6(ipv6) => ipv6.is_loopback(),
    }
}

/// Attempts to load config data from a toml file at the specified path else returns a default config.
///
/// Loads each variable individually to log all default values used and correctly parse hostnames.
pub fn load_config(file_path: &std::path::PathBuf) -> Result<IndexerConfig, IndexerError> {
    let default_config = IndexerConfig::default();

    if let Ok(contents) = std::fs::read_to_string(file_path) {
        let parsed_config: toml::Value = toml::from_str(&contents)
            .map_err(|e| IndexerError::ConfigError(format!("TOML parsing error: {}", e)))?;

        parse_field_or_warn_and_default!(
            parsed_config,
            grpc_listen_address,
            String,
            default_config,
            |val: String| match fetch_socket_addr_from_hostname(val.as_str()) {
                Ok(val) => Some(val),
                Err(_) => {
                    warn!("Invalid `grpc_listen_address`, using default.");
                    None
                }
            }
        );

        parse_field_or_warn_and_default!(parsed_config, grpc_tls, Boolean, default_config, Some);
        parse_field_or_warn_and_default!(
            parsed_config,
            tls_cert_path,
            String,
            default_config,
            |v| Some(Some(v))
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            tls_key_path,
            String,
            default_config,
            |v| Some(Some(v))
        );

        parse_field_or_warn_and_default!(
            parsed_config,
            validator_listen_address,
            String,
            default_config,
            |val: String| match fetch_socket_addr_from_hostname(val.as_str()) {
                Ok(val) => Some(val),
                Err(_) => {
                    warn!("Invalid `validator_listen_address`, using default.");
                    None
                }
            }
        );

        parse_field_or_warn_and_default!(
            parsed_config,
            validator_cookie_auth,
            Boolean,
            default_config,
            Some
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            validator_cookie_path,
            String,
            default_config,
            |v| Some(Some(v))
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            validator_user,
            String,
            default_config,
            |v| Some(Some(v))
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            validator_password,
            String,
            default_config,
            |v| Some(Some(v))
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            map_capacity,
            Integer,
            default_config,
            |v| Some(Some(v as usize))
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            map_shard_amount,
            Integer,
            default_config,
            |v| Some(Some(v as usize))
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            db_path,
            String,
            default_config,
            |v: String| {
                match PathBuf::from_str(v.as_str()) {
                    Ok(path) => Some(path),
                }
            }
        );
        parse_field_or_warn_and_default!(
            parsed_config,
            db_size,
            Integer,
            default_config,
            |v| Some(Some(v as usize))
        );
        parse_field_or_warn_and_default!(parsed_config, network, String, default_config, Some);
        parse_field_or_warn_and_default!(parsed_config, no_sync, Boolean, default_config, Some);
        parse_field_or_warn_and_default!(parsed_config, no_db, Boolean, default_config, Some);
        parse_field_or_warn_and_default!(parsed_config, no_state, Boolean, default_config, Some);

        let config = IndexerConfig {
            grpc_listen_address,
            grpc_tls,
            tls_cert_path,
            tls_key_path,
            validator_listen_address,
            validator_cookie_auth,
            validator_cookie_path,
            validator_user,
            validator_password,
            map_capacity,
            map_shard_amount,
            db_path,
            db_size,
            network,
            no_sync,
            no_db,
            no_state,
        };

        config.check_config()?;
        Ok(config)
    } else {
        warn!("Could not find config file at given path, using default config.");
        Ok(default_config)
    }
}
