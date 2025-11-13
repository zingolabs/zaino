//! Server configuration data.

use std::{net::SocketAddr, path::PathBuf};

use tonic::transport::{Identity, ServerTlsConfig};

use super::error::ServerError;

/// Settings for a Zaino configured with gRPC TLS: paths to key and certificate.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GrpcTls {
    /// Path to the TLS certificate file in PEM format.
    pub cert_path: PathBuf,
    /// Path to the TLS private key file in PEM format.
    pub key_path: PathBuf,
}

/// Configuration data for Zaino's gRPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GrpcServerConfig {
    /// gRPC server bind addr.
    pub listen_address: SocketAddr,
    /// Enables TLS.
    pub tls: Option<GrpcTls>,
}

impl GrpcServerConfig {
    /// If TLS is enabled, reads the certificate and key files and returns a valid
    /// `ServerTlsConfig`. If TLS is not enabled, returns `Ok(None)`.
    pub async fn get_valid_tls(&self) -> Result<Option<ServerTlsConfig>, ServerError> {
        match self.tls.clone() {
            Some(tls) => {
                if !tls.cert_path.exists() {
                    return Err(ServerError::ServerConfigError(
                        "TLS enabled but tls_cert_path does not exist".into(),
                    ));
                }
                let cert_path = tls.cert_path;

                if !tls.key_path.exists() {
                    return Err(ServerError::ServerConfigError(
                        "TLS enabled but tls_key_path does not exist".into(),
                    ));
                }
                let key_path = tls.key_path;
                let cert = tokio::fs::read(cert_path).await.map_err(|e| {
                    ServerError::ServerConfigError(format!("Failed to read TLS certificate: {e}"))
                })?;
                let key = tokio::fs::read(key_path).await.map_err(|e| {
                    ServerError::ServerConfigError(format!("Failed to read TLS key: {e}"))
                })?;
                let tls_id = Identity::from_pem(cert, key);
                let tls_config = ServerTlsConfig::new().identity(tls_id);
                Ok(Some(tls_config))
            }
            None => Ok(None),
        }
    }
}

/// Configuration data for Zaino's JSON RPC server, capable of servicing clients over TCP.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct JsonRpcServerConfig {
    /// Server bind addr.
    pub json_rpc_listen_address: SocketAddr,

    /// Enable cookie-based authentication with a valid `Some(<PathBuf>)` value: Directory to store authentication cookie file.
    /// An empty PathBuf that is still Some will have an emphemeral path assigned to it when zaino loads the config.
    #[serde(default)]
    pub cookie_dir: Option<PathBuf>,
}

impl Default for JsonRpcServerConfig {
    fn default() -> Self {
        Self {
            json_rpc_listen_address: "127.0.0.1:0".parse().unwrap(),
            cookie_dir: None,
        }
    }
}
