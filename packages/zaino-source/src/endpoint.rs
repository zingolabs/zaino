//! Transport-neutral validator link config: [`Endpoint`] (address, [`Auth`], [`Timeouts`],
//! [`LinkLimits`]) → URL + credentials, errors
//!
//! - no probe: reachability = the caller's retry loop (validator down at boot ≠ fatal)

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use crate::ResponseSizeCap;

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("validator address {address} is not host:port: {reason}")]
    Address { address: String, reason: String },

    #[error("cannot read validator cookie {path}: {source}")]
    Cookie { path: String, source: std::io::Error },

    #[error("cannot read indexer TLS {role} {path}: {source}")]
    TlsFile { role: &'static str, path: String, source: std::io::Error },
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

/// One validator's link: idle connections kept, largest reply buffered
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLimits {
    pub max_connections: NonZeroU32,
    pub response_size_cap: ResponseSizeCap,
}

/// 32 connections (zebrad admits 100 across every client), full reply cap
impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            max_connections: const { NonZeroU32::new(32).expect("32 is non-zero") },
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
    use std::io::Write as _;

    /// User always `__cookie__`, token trimmed with or without its prefix (packagers write both)
    #[test]
    fn cookie_credentials_are_the_cookie_user_and_its_trimmed_token() {
        for contents in ["__cookie__:sekrit", "  sekrit  \n"] {
            let mut file = tempfile::NamedTempFile::new().expect("temp file");
            write!(file, "{contents}").expect("write cookie");

            let auth =
                Auth::Cookie(file.path().to_owned()).credentials().map_err(|e| e.to_string());

            let token = Some(("__cookie__".to_owned(), "sekrit".to_owned()));
            assert_eq!(auth, Ok(token), "cookie file {contents:?}");
        }
    }

    #[test]
    fn an_unreadable_cookie_is_refused_naming_it() {
        let missing = Auth::Cookie(PathBuf::from("/nonexistent/cookie")).credentials();

        let named =
            matches!(&missing, Err(EndpointError::Cookie { path, .. }) if path.ends_with("cookie"));
        assert!(named, "/nonexistent/cookie: {missing:?}");
    }

    #[test]
    fn validator_url_keeps_the_host_and_rejects_non_host_port() {
        for (address, url) in [
            ("127.0.0.1:8232", "http://127.0.0.1:8232"),
            ("zebrad:18232", "http://zebrad:18232"),
            ("[::1]:8232", "http://[::1]:8232"),
        ] {
            assert_eq!(
                validator_url(address).map_err(|e| e.to_string()),
                Ok(url.into()),
                "{address}"
            );
        }
        for address in ["zebrad", "zebrad:", "zebrad:port", "not a host:8232", "zebrad:8232/path"] {
            let url = validator_url(address);
            assert!(matches!(url, Err(EndpointError::Address { .. })), "{address}: {url:?}");
        }
    }
}
