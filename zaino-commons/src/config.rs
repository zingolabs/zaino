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

/// Holds validator connection and authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ValidatorConfig {
    /// State service configuration
    pub config: ZainoStateConfig,
    /// Validator JsonRPC address.
    pub rpc_address: std::net::SocketAddr,
    /// Validator gRPC address.
    pub indexer_rpc_address: std::net::SocketAddr,
    /// Authentication method for RPC connections
    pub auth: AuthMethod,
}

impl ValidatorConfig {
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
                Ok((header_name, header_value)) => {
                    request_builder = request_builder.header(header_name, header_value);
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
        Self {
            config: ZainoStateConfig::default(),
            rpc_address: "127.0.0.1:8232".parse().expect("Valid socket address"),
            indexer_rpc_address: "127.0.0.1:8983".parse().expect("Valid socket address"),
            auth: AuthMethod::default(),
        }
    }
}

/// Authentication method for RPC connections.
///
/// This enum provides self-contained authentication handling,
/// including lazy loading of cookie files when needed.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// HTTP Basic authentication with username/password
    Basic { username: String, password: String },
    /// Cookie-based authentication (loads from file on demand)
    Cookie { path: std::path::PathBuf },
}

impl AuthMethod {
    /// Get authentication header for HTTP requests
    pub fn get_auth_header(&self) -> Result<(String, String), AuthError> {
        match self {
            AuthMethod::Basic { username, password } => {
                let credentials = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", username, password));
                Ok((
                    "Authorization".to_string(),
                    format!("Basic {}", credentials),
                ))
            }
            AuthMethod::Cookie { path } => {
                let cookie_token = read_cookie_token(path)?;
                let credentials = base64::engine::general_purpose::STANDARD
                    .encode(format!("__cookie__:{}", cookie_token));
                Ok((
                    "Authorization".to_string(),
                    format!("Basic {}", credentials),
                ))
            }
        }
    }
}

/// Shared cookie reading utility function
///
/// Reads and parses a cookie file, extracting the token part after "__cookie__:"
pub fn read_cookie_token(cookie_path: &std::path::Path) -> Result<String, AuthError> {
    let cookie_content =
        std::fs::read_to_string(cookie_path).map_err(|e| AuthError::CookieReadError(e))?;
    let trimmed_content = cookie_content.trim();
    if let Some(stripped) = trimmed_content.strip_prefix("__cookie__:") {
        Ok(stripped.to_string())
    } else {
        Err(AuthError::InvalidCookieFormat)
    }
}

impl Default for AuthMethod {
    fn default() -> Self {
        AuthMethod::Basic {
            username: "xxxxxx".to_string(),
            password: "xxxxxx".to_string(),
        }
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

/// Cookie-based authentication configuration for servers.
///
/// This is a simpler enum compared to AuthMethod, specifically for cases
/// where you only need to enable/disable cookie auth (like server configs).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CookieAuth {
    /// No cookie authentication
    Disabled,
    /// Cookie authentication enabled
    Enabled {
        /// Path to the cookie file
        path: PathBuf,
    },
}

impl CookieAuth {
    /// Get the cookie token if authentication is enabled, using shared cookie reading logic
    pub fn get_cookie_token(&self) -> Result<Option<String>, AuthError> {
        match self {
            CookieAuth::Disabled => Ok(None),
            CookieAuth::Enabled { path } => Ok(Some(read_cookie_token(path)?)),
        }
    }
}

impl Default for CookieAuth {
    fn default() -> Self {
        CookieAuth::Disabled
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
    /// Convert to Zebra's network type for internal use.
    pub fn to_zebra_network(&self) -> zebra_chain::parameters::Network {
        self.into()
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
            Network::Regtest => zebra_chain::parameters::Network::new_regtest(
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
                },
            ),
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

/// Zaino's wrapper for zebra_state configuration.
///
/// This provides a clean public API while maintaining compatibility
/// with Zebra's internal state configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ZainoStateConfig {
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

impl Default for ZainoStateConfig {
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

impl From<ZainoStateConfig> for zebra_state::Config {
    fn from(config: ZainoStateConfig) -> Self {
        zebra_state::Config {
            cache_dir: config.cache_dir,
            ephemeral: config.ephemeral,
            delete_old_database: config.delete_old_database,
            debug_stop_at_height: config.debug_stop_at_height,
            debug_validity_check_interval: config.debug_validity_check_interval,
        }
    }
}

impl From<zebra_state::Config> for ZainoStateConfig {
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
