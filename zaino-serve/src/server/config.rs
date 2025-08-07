//! Server configuration data.

use std::{net::SocketAddr, path::PathBuf};

use tonic::transport::{Identity, ServerTlsConfig};
use zaino_commons::config::CookieAuth;

use super::error::ServerError;

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

impl TlsConfig {
    /// Reads the certificate and key files and returns a ServerTlsConfig if TLS is enabled.
    /// Returns None if TLS is disabled.
    pub async fn get_server_tls_config(&self) -> Result<Option<ServerTlsConfig>, ServerError> {
        match self {
            TlsConfig::Disabled => Ok(None),
            TlsConfig::Enabled {
                cert_path,
                key_path,
            } => {
                // Read the certificate and key files asynchronously.
                let cert = tokio::fs::read(cert_path).await.map_err(|e| {
                    ServerError::ServerConfigError(format!(
                        "Failed to read TLS certificate from '{}': {}",
                        cert_path.display(),
                        e
                    ))
                })?;
                let key = tokio::fs::read(key_path).await.map_err(|e| {
                    ServerError::ServerConfigError(format!(
                        "Failed to read TLS key from '{}': {}",
                        key_path.display(),
                        e
                    ))
                })?;
                // Build the identity and TLS configuration.
                let identity = Identity::from_pem(cert, key);
                let tls_config = ServerTlsConfig::new().identity(identity);
                Ok(Some(tls_config))
            }
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig::Disabled
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

impl GrpcConfig {
    /// If TLS is enabled, reads the certificate and key files and returns a valid
    /// `ServerTlsConfig`. If TLS is not enabled, returns `Ok(None)`.
    pub async fn get_valid_tls(&self) -> Result<Option<ServerTlsConfig>, ServerError> {
        self.tls.get_server_tls_config().await
    }
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            listen_address: "127.0.0.1:8137".parse().expect("Valid socket address"),
            tls: TlsConfig::default(),
        }
    }
}

/// Configuration data for Zaino's JSON-RPC server.
pub struct JsonRpcConfig {
    /// Server bind address.
    pub listen_address: SocketAddr,

    /// Cookie-based authentication configuration.
    pub auth: CookieAuth,
}
