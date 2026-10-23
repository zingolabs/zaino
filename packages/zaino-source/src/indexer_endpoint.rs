//! zebrad indexer gRPC endpoint (plaintext or mutual TLS), shared by watch + block client

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

use crate::endpoint::validator_url;
use crate::{EndpointError, Timeouts};

/// Mutual TLS to a validator's indexer (zakurad: plaintext refused off loopback,
/// `[rpc.indexer_tls]`)
///
/// - `ca_path` verifies the server
/// - `cert_path` (leaf first) + `key_path` (PKCS#8, PKCS#1 or SEC1) = this client, signed by the
///   server's client CA
/// - `server_name` = the name on the server's certificate (`None` = the address's host)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZebraIndexerTls {
    pub ca_path: PathBuf,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub server_name: Option<String>,
}

/// `address`'s indexer endpoint, plaintext or mutual TLS, unprobed
///
/// - PEMs read here: unreadable = refused at boot, rotated file = read on restart
pub(crate) fn zebra_indexer_endpoint(
    address: &str,
    tls: Option<&ZebraIndexerTls>,
    timeouts: Timeouts,
) -> Result<Endpoint, EndpointError> {
    let invalid = |reason: String| EndpointError::Address { address: address.to_owned(), reason };
    let plain = validator_url(address)?;
    let endpoint = match tls {
        None => Endpoint::from_shared(plain).map_err(|e| invalid(e.to_string()))?,
        Some(tls) => {
            let read = |role: &'static str, path: &Path| {
                std::fs::read(path).map_err(|source| EndpointError::TlsFile {
                    role,
                    path: path.display().to_string(),
                    source,
                })
            };
            // validator_url's host:port only (no path, no query)
            let authority = &plain["http://".len()..];
            let host = authority.rsplit_once(':').map_or(authority, |(host, _)| host);
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let config = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(read("CA", &tls.ca_path)?))
                .identity(Identity::from_pem(
                    read("certificate", &tls.cert_path)?,
                    read("key", &tls.key_path)?,
                ))
                .domain_name(tls.server_name.as_deref().unwrap_or(host));
            let endpoint = Endpoint::from_shared(format!("https://{authority}"))
                .map_err(|e| invalid(e.to_string()))?;
            endpoint.tls_config(config).map_err(|e| invalid(format!("TLS: {e}")))?
        }
    };
    Ok(endpoint.connect_timeout(timeouts.connect).tcp_nodelay(true))
}
