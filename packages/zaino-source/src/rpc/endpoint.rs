//! Validator JSON-RPC endpoint from config: URL + the credentials it expects
//!
//! - no probe: reachability = the caller's retry loop (validator down at boot ≠ fatal)

use std::path::PathBuf;

use super::{LinkLimits, RpcError, Timeouts};

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("validator address {address} is not host:port: {reason}")]
    Address { address: String, reason: String },

    #[error("cannot read validator cookie {path}: {source}")]
    Cookie { path: String, source: std::io::Error },

    #[error("cannot read indexer TLS {role} {path}: {source}")]
    TlsFile { role: &'static str, path: String, source: std::io::Error },

    #[error("cannot build the validator RPC client: {0}")]
    Client(#[source] RpcError),
}

/// Validator JSON-RPC endpoint: `address` = `host:port`
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub address: String,
    pub auth: Auth,
    pub timeouts: Timeouts,
    pub limits: LinkLimits,
}

/// Credentials a validator expects
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    Cookie(PathBuf),
    Basic { user: String, password: String },
}

impl Auth {
    /// Basic-auth pair sent (`None` = no auth)
    ///
    /// - `__cookie__:` prefix stripped if present (older validators, some packagers omit it)
    pub(crate) fn credentials(&self) -> Result<Option<(String, String)>, EndpointError> {
        match self {
            Self::None => Ok(None),
            Self::Basic { user, password } => Ok(Some((user.clone(), password.clone()))),
            Self::Cookie(path) => {
                let contents = std::fs::read_to_string(path).map_err(|source| {
                    EndpointError::Cookie { path: path.display().to_string(), source }
                })?;
                let token = contents.trim();
                let token = token.strip_prefix("__cookie__:").unwrap_or(token);
                Ok(Some(("__cookie__".to_string(), token.to_string())))
            }
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
    use std::io::Write as _;

    /// - Cookie: user always `__cookie__`, token trimmed with or without its prefix (packagers
    ///   write both), unreadable = error
    /// - Basic = the pair, None = no auth
    #[test]
    fn credentials_come_from_the_cookie_or_the_pair() {
        let token = Some(("__cookie__".to_owned(), "sekrit".to_owned()));
        for contents in ["__cookie__:sekrit", "  sekrit  \n"] {
            let mut file = tempfile::NamedTempFile::new().expect("temp file");
            write!(file, "{contents}").expect("write cookie");
            let auth = Auth::Cookie(file.path().to_owned()).credentials().expect("cookie reads");
            assert_eq!(auth, token, "{contents:?}");
        }
        let basic = Auth::Basic { user: "user".to_owned(), password: "pass".to_owned() };
        let pair = Some(("user".to_owned(), "pass".to_owned()));
        assert_eq!(basic.credentials().expect("no file to read"), pair);
        assert_eq!(Auth::None.credentials().expect("no file to read"), None);
        let missing = Auth::Cookie(PathBuf::from("/nonexistent/cookie")).credentials();
        assert!(matches!(missing, Err(EndpointError::Cookie { .. })));
    }

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
