//! Transport-neutral validator link config: address → URL, [`Timeouts`], [`LinkLimits`], errors
//!
//! - no probe: reachability = the caller's retry loop (validator down at boot ≠ fatal)

use std::num::NonZeroU32;
use std::time::Duration;

use crate::{ResponseSizeCap, RpcError};

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("validator address {address} is not host:port: {reason}")]
    Address { address: String, reason: String },

    #[error("cannot read validator cookie {path}: {source}")]
    Cookie { path: String, source: std::io::Error },

    #[error("cannot read indexer TLS {role} {path}: {source}")]
    TlsFile { role: &'static str, path: String, source: std::io::Error },

    #[error("validator basic auth needs both user and password")]
    UnpairedCredentials,

    #[error("cannot build the validator RPC client: {0}")]
    Client(#[source] RpcError),
}

/// - `read` = silence, never total duration (multi-MB block over a slow link must finish)
/// - Total deadline below a block's transfer time = retry livelock
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub read: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { connect: Duration::from_secs(2), read: Duration::from_secs(30) }
    }
}

/// One validator's link: idle connections kept, response bytes per second (`None` = unlimited)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLimits {
    pub max_connections: NonZeroU32,
    pub max_bytes_per_sec: Option<NonZeroU32>,
    pub response_size_cap: ResponseSizeCap,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            max_connections: const { NonZeroU32::new(32).expect("32 is non-zero") },
            max_bytes_per_sec: None,
            response_size_cap: ResponseSizeCap::MAX,
        }
    }
}

/// `http://{address}`, hostname kept, not pre-resolved (reqwest resolves per connection →
/// follows a validator whose IP changes, e.g. a restarted pod)
pub(crate) fn validator_url(address: &str) -> Result<String, EndpointError> {
    let invalid = |reason: &str| EndpointError::Address {
        address: address.to_string(),
        reason: reason.to_string(),
    };
    let port_given = address.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if !port_given {
        return Err(invalid("missing or non-numeric port"));
    }
    let url =
        reqwest::Url::parse(&format!("http://{address}")).map_err(|e| invalid(&e.to_string()))?;
    if url.path() != "/" || url.query().is_some() {
        return Err(invalid("unexpected path or query"));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_url_keeps_the_host_and_rejects_non_host_port() {
        for (address, url) in [
            ("127.0.0.1:8232", "http://127.0.0.1:8232"),
            ("zebrad:18232", "http://zebrad:18232"),
            ("[::1]:8232", "http://[::1]:8232"),
        ] {
            assert_eq!(validator_url(address).expect(address), url);
        }
        for address in ["zebrad", "zebrad:", "zebrad:port", "not a host:8232", "zebrad:8232/path"] {
            let refused = matches!(validator_url(address), Err(EndpointError::Address { .. }));
            assert!(refused, "{address} accepted");
        }
    }
}
