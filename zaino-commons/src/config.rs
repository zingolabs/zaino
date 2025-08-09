//! Common configuration types shared across Zaino crates.

use base64::Engine;
use std::{net::SocketAddr, path::PathBuf};
use tonic::transport::{Identity, ServerTlsConfig};

/// Configuration-related errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// IO error reading configuration files
    #[error("IO error: {0}")]
    Io(String),
}

/// Network conversion errors
#[derive(Debug, thiserror::Error)]
pub enum NetworkConversionError {
    #[error(
        "Custom activation heights are only supported for regtest networks, but got {network:?}"
    )]
    CustomHeightsNotSupported { network: Network },
}

/// Holds validator connection and authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ValidatorConfig {
    Zebrad(ZebradConfig),
    Zcashd(ZcashdConfig),
}

/// Holds validator connection and authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZebradConfig {
    /// Validator JsonRPC address.
    pub rpc_address: std::net::SocketAddr,
    /// Authentication method for RPC connections
    pub auth: ZebradAuth,
    /// Backend configuration
    pub backend: ZebradBackend,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ZebradAuth {
    Disabled,
    Cookie(CookieAuth),
}

/// Holds validator connection and authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZcashdConfig {
    /// Validator JsonRPC address.
    pub rpc_address: std::net::SocketAddr,
    /// Authentication method for RPC connections
    pub auth: ZcashdAuth,
    /// Backend configuration
    pub backend: ZcashdBackend,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ZcashdAuth {
    Disabled,
    Password(PasswordAuth),
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct PasswordAuth {
    pub username: String,
    pub password: String,
}

impl PasswordAuth {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

impl ZebradConfig {
    /// Tests connection to the validator node and returns the correct URL.
    ///
    /// This method tries to connect to the validator using the configured RPC address
    /// and authentication, retrying up to 3 times with a 3-second delay between attempts.
    pub async fn test_and_get_url(&self) -> Result<reqwest::Url, std::io::Error> {
        use std::net::SocketAddr;

        let host = match self.rpc_address {
            SocketAddr::V4(_) => self.rpc_address.ip().to_string(),
            SocketAddr::V6(_) => format!("[{}]", self.rpc_address.ip()),
        };

        let url_string = format!("http://{}:{}", host, self.rpc_address.port());
        let url: reqwest::Url = url_string.parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid URL: {}", e),
            )
        })?;

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Client build error: {}", e),
                )
            })?;

        for attempt in 0..3 {
            let request_body = r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#;
            let mut request_builder = client
                .post(url.clone())
                .header("Content-Type", "application/json")
                .body(request_body);

            // Add authentication header if configured
            match self.auth.get_auth_header() {
                Ok(Some((header_name, header_value))) => {
                    request_builder = request_builder.header(header_name, header_value);
                }
                Ok(None) => {
                    // No authentication required
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("Authentication error: {}", e),
                    ));
                }
            }

            match request_builder.send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        let _body = response.bytes().await.map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("Response read error: {}", e),
                            )
                        })?;
                        return Ok(url);
                    }
                }
                Err(_) if attempt < 2 => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        format!("Connection failed after {} attempts: {}", attempt + 1, e),
                    ));
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Could not establish connection with validator node after 3 attempts",
        ))
    }
}

/// HTTP authentication header with semantic accessors
/// e.g. AuthHeader::new("Authorization", "Basic dXNlcjpwYXNz")
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthHeader {
    name: String,
    value: String,
}

impl AuthHeader {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    pub fn key(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Convenience method for Authorization headers
    pub fn authorization(value: impl Into<String>) -> Self {
        Self::new("Authorization", value)
    }
}

/// Authentication-related errors
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Failed to read cookie file: {0}")]
    CookieReadError(std::io::Error),
    #[error("Cookie file format is invalid (expected '__cookie__:token')")]
    InvalidCookieFormat,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub struct CookieAuth {
    pub path: PathBuf,
}

impl CookieAuth {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Get the cookie token
    pub fn read_cookie_token(&self) -> Result<String, AuthError> {
        let cookie_path: &std::path::Path = &self.path;
        let cookie_content =
            std::fs::read_to_string(cookie_path).map_err(|e| AuthError::CookieReadError(e))?;
        let trimmed_content = cookie_content.trim();

        if let Some(stripped) = trimmed_content.strip_prefix("__cookie__:") {
            Ok(stripped.to_string())
        } else {
            Err(AuthError::InvalidCookieFormat)
        }
    }
}

impl Default for CookieAuth {
    fn default() -> Self {
        Self {
            path: PathBuf::from(".cookie"),
        }
    }
}

impl ZebradAuth {
    /// Get authentication header for HTTP requests
    pub fn get_auth_header(&self) -> Result<Option<AuthHeader>, AuthError> {
        match self {
            ZebradAuth::Disabled => Ok(None),
            ZebradAuth::Cookie(cookie_auth) => {
                let cookie_token = cookie_auth.read_cookie_token()?;
                let credentials = base64::engine::general_purpose::STANDARD
                    .encode(format!("__cookie__:{}", cookie_token));
                Ok(Some(AuthHeader::authorization(format!("Basic {}", credentials))))
            }
        }
    }
}

impl Default for ZebradAuth {
    fn default() -> Self {
        ZebradAuth::Disabled
    }
}

impl ZcashdAuth {
    /// Get authentication header for HTTP requests
    pub fn get_auth_header(&self) -> Result<Option<AuthHeader>, AuthError> {
        match self {
            ZcashdAuth::Disabled => Ok(None),
            ZcashdAuth::Password(password_auth) => {
                let credentials = base64::engine::general_purpose::STANDARD.encode(format!(
                    "{}:{}",
                    password_auth.username, password_auth.password
                ));
                Ok(Some(AuthHeader::authorization(format!("Basic {}", credentials))))
            }
        }
    }
}

impl Default for ZcashdAuth {
    fn default() -> Self {
        ZcashdAuth::Disabled
    }
}

impl ZcashdConfig {
    /// Tests connection to the validator node and returns the correct URL.
    ///
    /// This method tries to connect to the validator using the configured RPC address
    /// and authentication, retrying up to 3 times with a 3-second delay between attempts.
    pub async fn test_and_get_url(&self) -> Result<reqwest::Url, std::io::Error> {
        use std::net::SocketAddr;

        let host = match self.rpc_address {
            SocketAddr::V4(_) => self.rpc_address.ip().to_string(),
            SocketAddr::V6(_) => format!("[{}]", self.rpc_address.ip()),
        };

        let url_string = format!("http://{}:{}", host, self.rpc_address.port());
        let url: reqwest::Url = url_string.parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid URL: {}", e),
            )
        })?;

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Client build error: {}", e),
                )
            })?;

        for attempt in 0..3 {
            let request_body = r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#;
            let mut request_builder = client
                .post(url.clone())
                .header("Content-Type", "application/json")
                .body(request_body);

            // Add authentication header if configured
            match self.auth.get_auth_header() {
                Ok(Some((header_name, header_value))) => {
                    request_builder = request_builder.header(header_name, header_value);
                }
                Ok(None) => {
                    // No authentication required
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("Authentication error: {}", e),
                    ));
                }
            }

            match request_builder.send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        let _body = response.bytes().await.map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("Response read error: {}", e),
                            )
                        })?;
                        return Ok(url);
                    }
                }
                Err(_) if attempt < 2 => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        format!("Connection failed after {} attempts: {}", attempt + 1, e),
                    ));
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Could not establish connection with validator node after 3 attempts",
        ))
    }
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        ValidatorConfig::Zebrad(ZebradConfig::default())
    }
}

impl Default for ZebradConfig {
    fn default() -> Self {
        Self {
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            auth: ZebradAuth::default(),
            backend: ZebradBackend::default(),
        }
    }
}

impl Default for ZcashdConfig {
    fn default() -> Self {
        Self {
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            auth: ZcashdAuth::default(),
            backend: ZcashdBackend::default(),
        }
    }
}

/// Holds service-level configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceConfig {
    /// StateService RPC timeout
    pub timeout: u32,
    /// StateService RPC max channel size.
    pub channel_size: u32,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            timeout: 30,
            channel_size: 32,
        }
    }
}

/// Holds cache configuration for DashMaps.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct CacheConfig {
    /// Capacity of the Dashmaps used for the Mempool and BlockCache NonFinalisedState.
    pub capacity: Option<usize>,
    /// Number of shard used in the DashMap used for the Mempool and BlockCache NonFinalisedState.
    ///
    /// shard_amount should greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    pub shard_amount: Option<usize>,
}

/// Holds database configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DatabaseConfig {
    /// Block Cache database file path.
    pub path: PathBuf,
    /// Block Cache database maximum size in gb.
    pub size: Option<usize>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./zaino_cache"),
            size: None,
        }
    }
}

/// Storage configuration for Zaino (shared across all backends)
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZainoStorageConfig {
    /// Cache configuration for DashMaps
    pub cache: CacheConfig,
    /// Zaino database configuration
    pub database: DatabaseConfig,
}

impl Default for ZainoStorageConfig {
    fn default() -> Self {
        Self {
            cache: CacheConfig::default(),
            database: DatabaseConfig::default(),
        }
    }
}

/// Zaino daemon's service configuration bundle.
///
/// This contains all configuration needed by Zaino's own services,
/// separate from validator-specific configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZainodServiceConfig {
    /// Service-level configuration (timeouts, channels)
    pub service: ServiceConfig,
    /// Cache configuration for DashMaps
    pub cache: CacheConfig,
    /// Zaino database configuration
    pub database: DatabaseConfig,
    /// Network type
    pub network: Network,
    /// Debug and testing configuration
    pub debug: DebugConfig,
}

impl Default for ZainodServiceConfig {
    fn default() -> Self {
        Self {
            service: ServiceConfig::default(),
            cache: CacheConfig::default(),
            database: DatabaseConfig::default(),
            network: Network::default(),
            debug: DebugConfig::default(),
        }
    }
}

/// Debug and testing configuration for Zaino daemon.
///
/// These settings are primarily used for testing and development scenarios.
/// In production, all options should typically remain at their default values (false).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DebugConfig {
    /// Skip blockchain synchronization on startup.
    ///
    /// When enabled, Zaino will start immediately without waiting for the validator
    /// to sync to the network tip. Useful for testing scenarios where you want to
    /// test Zaino functionality without a fully synced blockchain.
    pub no_sync: bool,

    /// Disable persistent storage.
    ///
    /// When enabled, Zaino will not persist blockchain data to disk and will operate
    /// in memory-only mode. This is useful for testing and development where you don't
    /// want to maintain blockchain state between runs.
    pub no_db: bool,

    /// Enable background database synchronization.
    ///
    /// When enabled, Zaino will sync its database in the background while serving
    /// requests, fetching data from the validator as needed.
    /// NOTE: This feature is currently unimplemented.
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

/// Type-safe configuration for Zebra daemon with State backend.
///
/// This configuration is guaranteed to only contain valid Zebra + State combinations,
/// making it impossible to construct invalid configurations at compile time.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZebradStateConfig {
    /// Zebra JsonRPC address
    pub rpc_address: std::net::SocketAddr,
    /// Zebra authentication configuration
    pub auth: ZebradAuth,
    /// Zebra state configuration
    pub zebra_state: ZebraStateConfig,
    /// Zebra gRPC address for state syncing
    pub indexer_rpc_address: std::net::SocketAddr,
    /// Zebra database configuration
    pub zebra_database: DatabaseConfig,
}

impl ZebradStateConfig {
    /// Extract from a ZebradConfig with State backend
    pub fn from_zebrad_state_backend(
        zebra_config: &ZebradConfig,
        state_config: &ZebraStateConfig,
        indexer_rpc_address: std::net::SocketAddr,
        zebra_database: &DatabaseConfig,
    ) -> Self {
        Self {
            rpc_address: zebra_config.rpc_address,
            auth: zebra_config.auth.clone(),
            zebra_state: state_config.clone(),
            indexer_rpc_address,
            zebra_database: zebra_database.clone(),
        }
    }
}

impl Default for ZebradStateConfig {
    fn default() -> Self {
        Self {
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            auth: ZebradAuth::default(),
            zebra_state: ZebraStateConfig::default(),
            indexer_rpc_address: "127.0.0.1:8983".parse().expect("Valid socket address"),
            zebra_database: DatabaseConfig::default(),
        }
    }
}

/// Type-safe validator configuration for Fetch backends only.
///
/// This enum ensures that only valid validator + Fetch backend combinations
/// can be constructed, preventing runtime errors from invalid configurations.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub enum ValidatorFetchConfig {
    /// Zebra daemon with Fetch backend
    Zebrad {
        /// Zebra JsonRPC address
        rpc_address: std::net::SocketAddr,
        /// Zebra authentication configuration
        auth: ZebradAuth,
    },
    /// Zcashd daemon with Fetch backend
    Zcashd {
        /// Zcashd JsonRPC address  
        rpc_address: std::net::SocketAddr,
        /// Zcashd authentication configuration
        auth: ZcashdAuth,
    },
}

impl Default for ValidatorFetchConfig {
    fn default() -> Self {
        ValidatorFetchConfig::Zebrad {
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            auth: ZebradAuth::default(),
        }
    }
}

/// Holds config data for `[ChainIndex]`.
/// TODO: Rename when ChainIndex update is complete.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct BlockCacheConfig {
    /// Cache configuration for DashMaps.
    pub cache: CacheConfig,
    /// Database configuration.
    pub database: DatabaseConfig,
    // todo! this porbably belongs in ValidatorConfig ... ?
    /// Network type.
    pub network: Network,
    /// Stops zaino waiting on server sync.
    /// Used for testing.
    pub no_sync: bool,
    /// Disables FinalisedState.
    /// Used for testing.
    pub no_db: bool,
}

// todo! analyse zaino native enum vs zingo-infra enum usage... I went for zaino native cause it made more sense for a public facing config enum... but maybe we could have gotten away with re-exporting the other one ??
// There's also the need for zebra network enum logic implementation which i think might have been impossible to do with the services' one (From<services::Network> for zebra::Network)
/// Network type for Zaino configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Mainnet network
    Mainnet,
    /// Testnet network
    Testnet,
    /// Regtest network (for local testing)
    Regtest,
}

impl Network {
    /// Convert to Zebra's network type using default configurations.
    pub fn to_zebra_default(&self) -> zebra_chain::parameters::Network {
        self.into()
    }

    /// Convert to Zebra's network type for internal use (alias for to_zebra_default).
    pub fn to_zebra_network(&self) -> zebra_chain::parameters::Network {
        self.to_zebra_default()
    }

    /// Convert to Zebra regtest network with custom activation heights.
    /// Returns an error if called on non-regtest networks.
    pub fn to_zebra_regtest_with_heights(
        &self,
        heights: zebra_chain::parameters::testnet::ConfiguredActivationHeights,
    ) -> Result<zebra_chain::parameters::Network, NetworkConversionError> {
        match self {
            Network::Regtest => Ok(zebra_chain::parameters::Network::new_regtest(heights)),
            network => Err(NetworkConversionError::CustomHeightsNotSupported { network: *network }),
        }
    }

    /// Get the standard regtest activation heights used by Zaino.
    pub fn zaino_regtest_heights() -> zebra_chain::parameters::testnet::ConfiguredActivationHeights
    {
        zebra_chain::parameters::testnet::ConfiguredActivationHeights {
            before_overwinter: Some(1),
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(1),
            nu6: Some(1),
            nu6_1: None,
            nu7: None,
        }
    }

    /// Determines if sync should be skipped for testing.
    ///
    /// - Mainnet/Testnet: Skip sync (false) because we don't want to sync real chains in tests
    /// - Regtest: Enable sync (true) because regtest is local and fast to sync
    pub fn should_sync_for_testing(&self) -> bool {
        match self {
            Network::Mainnet | Network::Testnet => false, // Real networks - don't sync in tests
            Network::Regtest => true,                     // Local network - safe and fast to sync
        }
    }
}

impl Into<zingo_infra_services::network::Network> for Network {
    fn into(self) -> zingo_infra_services::network::Network {
        match self {
            Network::Mainnet => zingo_infra_services::network::Network::Mainnet,
            Network::Regtest => zingo_infra_services::network::Network::Regtest,
            Network::Testnet => zingo_infra_services::network::Network::Testnet,
        }
    }
}

impl From<zingo_infra_services::network::Network> for Network {
    fn from(value: zingo_infra_services::network::Network) -> Self {
        match value {
            zingo_infra_services::network::Network::Regtest => Network::Regtest,
            zingo_infra_services::network::Network::Testnet => Network::Testnet,
            zingo_infra_services::network::Network::Mainnet => Network::Mainnet,
        }
    }
}

impl From<zebra_chain::parameters::Network> for Network {
    fn from(value: zebra_chain::parameters::Network) -> Self {
        match value {
            zebra_chain::parameters::Network::Mainnet => Network::Mainnet,
            zebra_chain::parameters::Network::Testnet(parameters) => {
                if parameters.is_regtest() {
                    Network::Regtest
                } else {
                    Network::Regtest
                }
            }
        }
    }
}

impl Into<zebra_chain::parameters::Network> for Network {
    fn into(self) -> zebra_chain::parameters::Network {
        match self {
            Network::Regtest => {
                zebra_chain::parameters::Network::new_regtest(Self::zaino_regtest_heights())
            }
            Network::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
            Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
        }
    }
}

impl Into<zebra_chain::parameters::Network> for &Network {
    fn into(self) -> zebra_chain::parameters::Network {
        (*self).into()
    }
}

// TODO: Implement From<zebra_chain::parameters::Network> for Network
// This is complex due to zebra's network structure - will implement later

impl Default for Network {
    fn default() -> Self {
        Network::Testnet
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
/// Type of backend to be used.
pub enum BackendType {
    /// Uses ReadStateService (Zebra)
    State,
    /// Uses JsonRPC client (Zcashd. Zainod)
    Fetch,
}

/// Backend configuration specific to Zebra validator.
/// Zebra supports both State (ReadStateService) and Fetch (JsonRPC) backends.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ZebradBackend {
    /// State backend using Zebra's ReadStateService
    State {
        /// Zebra state configuration
        state_config: ZebraStateConfig,
        /// Zebra gRPC address for state syncing
        indexer_rpc_address: std::net::SocketAddr,
        /// Zebra database configuration
        zebra_database: DatabaseConfig,
    },
    /// Fetch backend using JsonRPC
    Fetch,
}

/// Backend configuration specific to Zcashd validator.
/// Zcashd only supports Fetch (JsonRPC) backend.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ZcashdBackend {
    /// Fetch backend using JsonRPC (only option for Zcashd)
    Fetch,
}

impl Default for ZebradBackend {
    fn default() -> Self {
        ZebradBackend::Fetch
    }
}

impl Default for ZcashdBackend {
    fn default() -> Self {
        ZcashdBackend::Fetch
    }
}

/// Zaino's wrapper for Zebra's state configuration.
///
/// This is NOT a 1-on-1 mapping with `zebra_state::Config`. Instead, it's a deliberate
/// abstraction layer that:
/// - Controls exactly which Zebra state functionality we expose to consumers
/// - Protects us from breaking changes in Zebra's public API
/// - Allows us to maintain our own stable configuration interface
/// - Forces explicit updates to conversion logic when Zebra changes
///
/// Conversions to/from `zebra_state::Config` are maintained below to ensure flexibility.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZebraStateConfig {
    /// Path to the directory for cached blockchain state
    pub cache_dir: std::path::PathBuf,
    /// If true, the state is stored in memory only and not persisted
    pub ephemeral: bool,
    /// If true, delete old database files on startup
    pub delete_old_database: bool,
    /// Optional height to stop processing blocks (for debugging)
    pub debug_stop_at_height: Option<u32>,
    /// Optional interval for validity checks (for debugging), e.g. "30s", "5min"
    #[serde(with = "humantime_serde")]
    pub debug_validity_check_interval: Option<std::time::Duration>,
}

impl Default for ZebraStateConfig {
    fn default() -> Self {
        Self {
            cache_dir: std::path::PathBuf::from("./zaino_state_cache"),
            ephemeral: false,
            delete_old_database: false,
            debug_stop_at_height: None,
            debug_validity_check_interval: None,
        }
    }
}

impl From<ZebraStateConfig> for zebra_state::Config {
    fn from(config: ZebraStateConfig) -> Self {
        zebra_state::Config {
            cache_dir: config.cache_dir,
            ephemeral: config.ephemeral,
            delete_old_database: config.delete_old_database,
            debug_stop_at_height: config.debug_stop_at_height,
            debug_validity_check_interval: config.debug_validity_check_interval,
        }
    }
}

impl From<zebra_state::Config> for ZebraStateConfig {
    fn from(config: zebra_state::Config) -> Self {
        Self {
            cache_dir: config.cache_dir,
            ephemeral: config.ephemeral,
            delete_old_database: config.delete_old_database,
            debug_stop_at_height: config.debug_stop_at_height,
            debug_validity_check_interval: config.debug_validity_check_interval,
        }
    }
}

/// TLS configuration for gRPC server.
///
/// This enum provides lazy loading of certificate and key files when TLS is enabled.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsConfig {
    /// TLS is disabled
    Disabled,
    /// TLS is enabled with certificate and key file paths
    Enabled {
        /// Path to the TLS certificate file in PEM format
        cert_path: PathBuf,
        /// Path to the TLS private key file in PEM format  
        key_path: PathBuf,
    },
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig::Disabled
    }
}

impl TlsConfig {
    /// Reads the certificate and key files and returns a ServerTlsConfig if TLS is enabled.
    /// Returns None if TLS is disabled.
    pub async fn get_server_tls_config(&self) -> Result<Option<ServerTlsConfig>, ConfigError> {
        match self {
            TlsConfig::Disabled => Ok(None),
            TlsConfig::Enabled {
                cert_path,
                key_path,
            } => {
                // Read the certificate and key files asynchronously.
                let cert = tokio::fs::read(cert_path).await.map_err(|e| {
                    ConfigError::Io(format!(
                        "Failed to read TLS certificate from '{}': {}",
                        cert_path.display(),
                        e
                    ))
                })?;
                let key = tokio::fs::read(key_path).await.map_err(|e| {
                    ConfigError::Io(format!(
                        "Failed to read TLS key from '{}': {}",
                        key_path.display(),
                        e
                    ))
                })?;

                // Create the TLS identity and server configuration.
                let identity = Identity::from_pem(&cert, &key);
                Ok(Some(ServerTlsConfig::new().identity(identity)))
            }
        }
    }
}

/// Configuration data for Zaino's JSON-RPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct JsonRpcConfig {
    /// Server bind address.
    pub listen_address: SocketAddr,
    /// Cookie-based authentication configuration.
    pub auth: CookieAuth,
}

impl Default for JsonRpcConfig {
    fn default() -> Self {
        Self {
            listen_address: "127.0.0.1:8237".parse().expect("Valid socket address"),
            auth: CookieAuth::default(),
        }
    }
}

/// Configuration data for Zaino's gRPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GrpcConfig {
    /// gRPC server bind address.
    pub listen_address: SocketAddr,
    /// TLS configuration.
    pub tls: TlsConfig,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            listen_address: "127.0.0.1:8137".parse().expect("Valid socket address"),
            tls: TlsConfig::default(),
        }
    }
}

impl GrpcConfig {
    /// If TLS is enabled, reads the certificate and key files and returns a valid
    /// `ServerTlsConfig`. If TLS is not enabled, returns `Ok(None)`.
    pub async fn get_valid_tls(&self) -> Result<Option<ServerTlsConfig>, ConfigError> {
        self.tls.get_server_tls_config().await
    }
}
