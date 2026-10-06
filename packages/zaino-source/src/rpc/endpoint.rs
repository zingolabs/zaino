//! A validator's JSON-RPC endpoint from config: its URL and the credentials it expects
//!
//! - no probe: reachability = the caller's retry loop (a validator down at boot is not fatal)

use std::path::Path;

use super::RpcError;

/// Why a configured validator endpoint is unusable.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// The configured address is not a `host:port`.
    #[error("validator address {address} is not host:port: {reason}")]
    Address {
        /// The address as configured.
        address: String,
        /// What is wrong with it.
        reason: String,
    },

    /// The cookie file could not be read.
    #[error("cannot read validator cookie {path}: {source}")]
    Cookie {
        /// Path to the cookie file.
        path: String,
        /// The underlying read failure.
        source: std::io::Error,
    },

    /// The client could not be constructed.
    #[error("cannot build the validator RPC client: {0}")]
    Client(#[source] RpcError),
}

/// Reads the credentials a validator expects from the configured parts.
///
/// A cookie path wins over an explicit user/password pair: a validator
/// configured for cookie auth will reject the pair. The cookie file's
/// `__cookie__:` prefix is stripped when present and tolerated when absent,
/// which is how older validators and some packagers write it.
pub(crate) fn auth_from_parts(
    cookie_path: Option<&Path>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Option<(String, String)>, EndpointError> {
    match cookie_path {
        Some(path) => {
            let contents = std::fs::read_to_string(path).map_err(|source| {
                EndpointError::Cookie { path: path.display().to_string(), source }
            })?;
            let token = contents.trim();
            let token = token.strip_prefix("__cookie__:").unwrap_or(token);
            Ok(Some(("__cookie__".to_string(), token.to_string())))
        }
        None => Ok(Some((
            user.unwrap_or_else(|| "xxxxxx".to_string()),
            password.unwrap_or_else(|| "xxxxxx".to_string()),
        ))),
    }
}

/// `http://{address}`; hostname kept, not pre-resolved (reqwest resolves per connection → follows a
/// validator whose IP changes, e.g. a restarted pod)
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

    /// The cookie's `__cookie__:` prefix is stripped, and the username is the
    /// literal `__cookie__` rather than anything configured — a validator on
    /// cookie auth accepts no other user.
    #[test]
    fn cookie_auth_strips_the_prefix() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        write!(file, "__cookie__:sekrit").expect("write cookie");

        let auth = auth_from_parts(Some(file.path()), None, None).expect("cookie reads");
        assert_eq!(auth, Some(("__cookie__".to_string(), "sekrit".to_string())));
    }

    /// Some packagers write the token without the prefix. Treating that as part
    /// of the token would send the wrong password and fail authentication.
    #[test]
    fn a_bare_cookie_token_is_accepted() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        writeln!(file, "  sekrit  ").expect("write cookie");

        let auth = auth_from_parts(Some(file.path()), None, None).expect("cookie reads");
        assert_eq!(auth, Some(("__cookie__".to_string(), "sekrit".to_string())));
    }

    /// A cookie path wins over an explicit pair: a validator configured for
    /// cookie auth rejects the pair, so preferring it would fail every request.
    #[test]
    fn a_cookie_path_wins_over_an_explicit_pair() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        write!(file, "__cookie__:sekrit").expect("write cookie");

        let auth =
            auth_from_parts(Some(file.path()), Some("user".to_string()), Some("pass".to_string()))
                .expect("cookie reads");

        assert_eq!(auth, Some(("__cookie__".to_string(), "sekrit".to_string())));
    }

    #[test]
    fn an_explicit_pair_is_used_when_there_is_no_cookie() {
        let pair = (Some("user".to_string()), Some("pass".to_string()));
        let auth = auth_from_parts(None, pair.0, pair.1).expect("no file to read");
        assert_eq!(auth, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn a_missing_cookie_file_is_reported() {
        let missing = auth_from_parts(Some(Path::new("/nonexistent/cookie")), None, None);
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
