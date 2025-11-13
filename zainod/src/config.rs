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
/// If the file cannot be read, or if its contents cannot be parsed into `ZainodConfig`,
/// a warning is logged, and a default configuration is returned.
/// Finally, there is an override of the config using environmental variables.
/// The loaded or default configuration undergoes further checks and finalization.
pub fn load_config(file_path: &PathBuf) -> Result<ZainodConfig, IndexerError> {
    // Configuration sources are layered: Env > TOML > Defaults.
    let figment = Figment::new()
        // 1. Base defaults from `ZainodConfig::default()`.
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
            zaino_state::BackendType::State => {
                Ok(BackendConfig::State(StateServiceConfig::from(cfg)))
            }
            zaino_state::BackendType::Fetch => {
                Ok(BackendConfig::Fetch(FetchServiceConfig::from(cfg)))
            }
        }
    }
}

#[allow(deprecated)]
impl From<ZainodConfig> for StateServiceConfig {
    fn from(cfg: ZainodConfig) -> Self {
        StateServiceConfig {
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
        }
    }
}

#[allow(deprecated)]
impl From<ZainodConfig> for FetchServiceConfig {
    fn from(cfg: ZainodConfig) -> Self {
        FetchServiceConfig {
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
#[cfg(test)]
mod test {
    use crate::error::IndexerError;

    use super::ZainodConfig;

    use super::load_config;

    use figment::Jail;

    use std::path::PathBuf;

    use zaino_common::{DatabaseSize, Network};

    // Use the explicit library name `zainodlib` as defined in Cargo.toml [lib] name.

    // If BackendType is used directly in assertions beyond what IndexerConfig holds:
    use zaino_state::BackendType as ZainoBackendType;

    #[test]
    // Validates loading a valid configuration via `load_config`,
    // ensuring fields are parsed and `check_config` passes with mocked prerequisite files.
    pub(crate) fn test_deserialize_full_valid_config() {
        Jail::expect_with(|jail| {
            // Define RELATIVE paths/filenames for use within the jail
            let cert_file_name = "test_cert.pem";
            let key_file_name = "test_key.pem";
            let validator_cookie_file_name = "validator.cookie";
            let zaino_cookie_dir_name = "zaino_cookies_dir";
            let zaino_db_dir_name = "zaino_db_dir";
            let zebra_db_dir_name = "zebra_db_dir";

            // Create the directories within the jail FIRST
            jail.create_dir(zaino_cookie_dir_name)?;
            jail.create_dir(zaino_db_dir_name)?;
            jail.create_dir(zebra_db_dir_name)?;

            // Use relative paths in the TOML string
            let toml_str = format!(
                r#"
            backend = "fetch"
            storage.database.path = "{zaino_db_dir_name}"
            zebra_db_path = "{zebra_db_dir_name}"
            db_size = 100
            network = "Mainnet"
            no_db = false
            slow_sync = false

            [validator_settings]
            validator_jsonrpc_listen_address = "192.168.1.10:18232"
            validator_cookie_path = "{validator_cookie_file_name}"
            validator_user = "user"
            validator_password = "password"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8000"
            cookie_dir = "{zaino_cookie_dir_name}"

            [grpc_settings]
            listen_address = "0.0.0.0:9000"

            [grpc_settings.tls]
            cert_path = "{cert_file_name}"
            key_path = "{key_file_name}"
        "#
            );

            let temp_toml_path = jail.directory().join("full_config.toml");
            jail.create_file(&temp_toml_path, &toml_str)?;

            // Create the actual mock files within the jail using the relative names
            jail.create_file(cert_file_name, "mock cert content")?;
            jail.create_file(key_file_name, "mock key content")?;
            jail.create_file(validator_cookie_file_name, "mock validator cookie content")?;

            let config_result = load_config(&temp_toml_path);
            assert!(
                config_result.is_ok(),
                "load_config failed: {:?}",
                config_result.err()
            );
            let finalized_config = config_result.unwrap();

            assert_eq!(finalized_config.backend, ZainoBackendType::Fetch);
            assert!(finalized_config.json_server_settings.is_some());
            assert_eq!(
                finalized_config
                    .json_server_settings
                    .as_ref()
                    .expect("json settings to be Some")
                    .json_rpc_listen_address,
                "127.0.0.1:8000".parse().unwrap()
            );
            assert_eq!(
                finalized_config
                    .json_server_settings
                    .as_ref()
                    .expect("json settings to be Some")
                    .cookie_dir,
                Some(PathBuf::from(zaino_cookie_dir_name))
            );
            assert_eq!(
                finalized_config
                    .clone()
                    .grpc_settings
                    .tls
                    .expect("tls to be Some in finalized conifg")
                    .cert_path,
                PathBuf::from(cert_file_name)
            );
            assert_eq!(
                finalized_config
                    .clone()
                    .grpc_settings
                    .tls
                    .expect("tls to be Some in finalized_conifg")
                    .key_path,
                PathBuf::from(key_file_name)
            );
            assert_eq!(
                finalized_config.validator_settings.validator_cookie_path,
                Some(PathBuf::from(validator_cookie_file_name))
            );
            assert_eq!(
                finalized_config.storage.database.path,
                PathBuf::from(zaino_db_dir_name)
            );
            assert_eq!(
                finalized_config.zebra_db_path,
                PathBuf::from(zebra_db_dir_name)
            );
            assert_eq!(finalized_config.network, Network::Mainnet);
            assert_eq!(
                finalized_config.grpc_settings.listen_address,
                "0.0.0.0:9000".parse().unwrap()
            );
            assert!(finalized_config.grpc_settings.tls.is_some());
            assert_eq!(
                finalized_config.validator_settings.validator_user,
                Some("user".to_string())
            );
            assert_eq!(
                finalized_config.validator_settings.validator_password,
                Some("password".to_string())
            );
            assert_eq!(finalized_config.storage.cache.capacity, 10000);
            assert_eq!(finalized_config.storage.cache.shard_count(), 16);
            assert_eq!(
                finalized_config.storage.database.size.to_byte_count(),
                128 * 1024 * 1024 * 1024
            );
            assert!(match finalized_config.storage.database.size {
                DatabaseSize::Gb(0) => false,
                DatabaseSize::Gb(_) => true,
            });

            Ok(())
        });
    }

    #[test]
    // Verifies that when optional fields are omitted from TOML, `load_config` ensures they correctly adopt default values.
    pub(crate) fn test_deserialize_optional_fields_missing() {
        Jail::expect_with(|jail| {
            let toml_str = r#"
            backend = "state"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/opt/zaino/data"
            zebra_db_path = "/opt/zebra/data"
            network = "Testnet"
        "#;
            let temp_toml_path = jail.directory().join("optional_missing.toml");
            jail.create_file(&temp_toml_path, toml_str)?;

            let config = load_config(&temp_toml_path).expect("load_config failed");
            let default_values = ZainodConfig::default();

            assert_eq!(config.backend, ZainoBackendType::State);
            assert_eq!(
                config.json_server_settings.is_some(),
                default_values.json_server_settings.is_some()
            );
            assert_eq!(
                config.validator_settings.validator_user,
                default_values.validator_settings.validator_user
            );
            assert_eq!(
                config.validator_settings.validator_password,
                default_values.validator_settings.validator_password
            );
            assert_eq!(
                config.storage.cache.capacity,
                default_values.storage.cache.capacity
            );
            assert_eq!(
                config.storage.cache.shard_count(),
                default_values.storage.cache.shard_count(),
            );
            assert_eq!(
                config.storage.database.size,
                default_values.storage.database.size
            );
            assert_eq!(
                config.storage.database.size.to_byte_count(),
                default_values.storage.database.size.to_byte_count()
            );
            Ok(())
        });
    }

    #[test]
    // Tests the logic (via `load_config` and its internal call to `finalize_config_logic`)
    // for setting `cookie_dir` based on `enable_cookie_auth`.
    pub(crate) fn test_cookie_dir_logic() {
        Jail::expect_with(|jail| {
            // Scenario 1: auth enabled, cookie_dir missing (should use default ephemeral path)
            let s1_path = jail.directory().join("s1.toml");
            jail.create_file(
                &s1_path,
                r#"
            backend = "fetch"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"
            cookie_dir = ""

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
            )?;

            let config1 = load_config(&s1_path).expect("Config S1 failed");
            assert!(config1.json_server_settings.is_some());
            assert!(config1
                .json_server_settings
                .as_ref()
                .expect("json settings is Some")
                .cookie_dir
                .is_some());

            // Scenario 2: auth enabled, cookie_dir specified
            let s2_path = jail.directory().join("s2.toml");
            jail.create_file(
                &s2_path,
                r#"
            backend = "fetch"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"
            cookie_dir = "/my/cookie/path"

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
            )?;
            let config2 = load_config(&s2_path).expect("Config S2 failed");
            assert!(config2.json_server_settings.is_some());
            assert_eq!(
                config2
                    .json_server_settings
                    .as_ref()
                    .expect("json settings to be Some")
                    .cookie_dir,
                Some(PathBuf::from("/my/cookie/path"))
            );
            let s3_path = jail.directory().join("s3.toml");
            jail.create_file(
                &s3_path,
                r#"
            backend = "fetch"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
            )?;
            let config3 = load_config(&s3_path).expect("Config S3 failed");
            assert!(config3
                .json_server_settings
                .expect("json server settings to unwrap in config S3")
                .cookie_dir
                .is_none());
            Ok(())
        });
    }

    #[test]
    pub(crate) fn test_string_none_as_path_for_cookie_dir() {
        Jail::expect_with(|jail| {
            let toml_auth_enabled_path = jail.directory().join("auth_enabled.toml");
            // cookie auth on but no dir assigned
            jail.create_file(
                &toml_auth_enabled_path,
                r#"
            backend = "fetch"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"
            cookie_dir = ""
        "#,
            )?;
            let config_auth_enabled =
                load_config(&toml_auth_enabled_path).expect("Auth enabled failed");
            assert!(config_auth_enabled.json_server_settings.is_some());
            assert!(config_auth_enabled
                .json_server_settings
                .as_ref()
                .expect("json settings to be Some")
                .cookie_dir
                .is_some());

            // omitting cookie_dir will set it to None
            let toml_auth_disabled_path = jail.directory().join("auth_disabled.toml");
            jail.create_file(
                &toml_auth_disabled_path,
                r#"
            backend = "fetch"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
            )?;
            let config_auth_disabled =
                load_config(&toml_auth_disabled_path).expect("Auth disabled failed");
            assert!(config_auth_disabled.json_server_settings.is_some());
            assert_eq!(
                config_auth_disabled
                    .json_server_settings
                    .as_ref()
                    .expect("json settings to be Some")
                    .cookie_dir,
                None
            );
            Ok(())
        });
    }

    #[test]
    // Checks that `load_config` with an empty TOML string results in the default `IndexerConfig` values.
    pub(crate) fn test_deserialize_empty_string_yields_default() {
        Jail::expect_with(|jail| {
            let empty_toml_path = jail.directory().join("empty.toml");
            jail.create_file(&empty_toml_path, "")?;
            let config = load_config(&empty_toml_path).expect("Empty TOML load failed");
            let default_config = ZainodConfig::default();
            // Compare relevant fields that should come from default
            assert_eq!(config.network, default_config.network);
            assert_eq!(config.backend, default_config.backend);
            assert_eq!(
                config.json_server_settings.is_some(),
                default_config.json_server_settings.is_some()
            );
            assert_eq!(
                config.validator_settings.validator_user,
                default_config.validator_settings.validator_user
            );
            assert_eq!(
                config.validator_settings.validator_password,
                default_config.validator_settings.validator_password
            );
            assert_eq!(
                config.storage.cache.capacity,
                default_config.storage.cache.capacity
            );
            assert_eq!(
                config.storage.cache.shard_count(),
                default_config.storage.cache.shard_count()
            );
            assert_eq!(
                config.storage.database.size,
                default_config.storage.database.size
            );
            assert_eq!(
                config.storage.database.size.to_byte_count(),
                default_config.storage.database.size.to_byte_count()
            );
            Ok(())
        });
    }

    #[test]
    // Ensures `load_config` returns an error for an invalid `backend` type string in TOML.
    pub(crate) fn test_deserialize_invalid_backend_type() {
        Jail::expect_with(|jail| {
            let invalid_toml_path = jail.directory().join("invalid_backend.toml");
            jail.create_file(&invalid_toml_path, r#"backend = "invalid_type""#)?;
            let result = load_config(&invalid_toml_path);
            assert!(result.is_err());
            if let Err(IndexerError::ConfigError(msg)) = result {
                assert!(msg.contains("Invalid backend type"));
            }
            Ok(())
        });
    }

    #[test]
    // Ensures `load_config` returns an error for an invalid socket address string in TOML.
    pub(crate) fn test_deserialize_invalid_socket_address() {
        Jail::expect_with(|jail| {
            let invalid_toml_path = jail.directory().join("invalid_socket.toml");
            jail.create_file(
                &invalid_toml_path,
                r#"
            [json_server_settings]
            json_rpc_listen_address = "not-a-valid-address"
            cookie_dir = ""
            "#,
            )?;
            let result = load_config(&invalid_toml_path);
            assert!(result.is_err());
            if let Err(IndexerError::ConfigError(msg)) = result {
                assert!(msg.contains("invalid socket address syntax"));
            }
            Ok(())
        });
    }

    #[test]
    // Validates that the actual zindexer.toml file (with optional values commented out)
    // is parsed correctly by `load_config`, applying defaults for missing optional fields.
    pub(crate) fn test_parse_zindexer_toml_integration() {
        let zindexer_toml_content = include_str!("../zindexer.toml");

        Jail::expect_with(|jail| {
            let temp_toml_path = jail.directory().join("zindexer_test.toml");
            jail.create_file(&temp_toml_path, zindexer_toml_content)?;

            let config_result = load_config(&temp_toml_path);
            assert!(
                config_result.is_ok(),
                "load_config failed to parse zindexer.toml: {:?}",
                config_result.err()
            );
            let config = config_result.unwrap();
            let defaults = ZainodConfig::default();

            assert_eq!(config.backend, ZainoBackendType::Fetch);
            assert_eq!(
                config.validator_settings.validator_user,
                defaults.validator_settings.validator_user
            );

            Ok(())
        });
    }

    // Figment-specific tests below are generally self-descriptive by name
    #[test]
    pub(crate) fn test_figment_env_override_toml_and_defaults() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test_config.toml",
                r#"
            network = "Testnet"
        "#,
            )?;
            jail.set_env("ZAINO_NETWORK", "Mainnet");
            jail.set_env(
                "ZAINO_JSON_SERVER_SETTINGS-JSON_RPC_LISTEN_ADDRESS",
                "127.0.0.1:0",
            );
            jail.set_env("ZAINO_JSON_SERVER_SETTINGS-COOKIE_DIR", "/env/cookie/path");
            jail.set_env("ZAINO_STORAGE.CACHE.CAPACITY", "12345");

            let temp_toml_path = jail.directory().join("test_config.toml");
            let config = load_config(&temp_toml_path).expect("load_config should succeed");

            assert_eq!(config.network, Network::Mainnet);
            assert_eq!(config.storage.cache.capacity, 12345);
            assert!(config.json_server_settings.is_some());
            assert_eq!(
                config
                    .json_server_settings
                    .as_ref()
                    .expect("json settings to be Some")
                    .cookie_dir,
                Some(PathBuf::from("/env/cookie/path"))
            );
            assert!(config.grpc_settings.tls.is_none());
            Ok(())
        });
    }

    #[test]
    pub(crate) fn test_figment_toml_overrides_defaults() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test_config.toml",
                r#"
            network = "Regtest"

            [json_server_settings]
            json_rpc_listen_address = ""
            cookie_dir = ""
        "#,
            )?;
            let temp_toml_path = jail.directory().join("test_config.toml");
            // a json_server_setting without a listening address is forbidden
            assert!(load_config(&temp_toml_path).is_err());
            Ok(())
        });
    }

    #[test]
    pub(crate) fn test_figment_all_defaults() {
        Jail::expect_with(|jail| {
            jail.create_file("empty_config.toml", "")?;
            let temp_toml_path = jail.directory().join("empty_config.toml");
            let config =
                load_config(&temp_toml_path).expect("load_config should succeed with empty toml");
            let defaults = ZainodConfig::default();
            assert_eq!(config.network, defaults.network);
            assert_eq!(
                config.json_server_settings.is_some(),
                defaults.json_server_settings.is_some()
            );
            assert_eq!(
                config.storage.cache.capacity,
                defaults.storage.cache.capacity
            );
            Ok(())
        });
    }

    #[test]
    pub(crate) fn test_figment_invalid_env_var_type() {
        Jail::expect_with(|jail| {
            jail.create_file("test_config.toml", "")?;
            jail.set_env("ZAINO_STORAGE.CACHE.CAPACITY", "not_a_number");
            let temp_toml_path = jail.directory().join("test_config.toml");
            let result = load_config(&temp_toml_path);
            assert!(result.is_err());
            if let Err(IndexerError::ConfigError(msg)) = result {
                assert!(msg.to_lowercase().contains("storage.cache.capacity") && msg.contains("invalid type"),
                        "Error message should mention 'map_capacity' (case-insensitive) and 'invalid type'. Got: {msg}");
            } else {
                panic!("Expected ConfigError, got {result:?}");
            }
            Ok(())
        });
    }
}
