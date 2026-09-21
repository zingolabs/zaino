//! Resolving and reaching a validator's JSON-RPC endpoint.
//!
//! Two things every caller of this crate needs before it can make a request:
//! the credentials the validator expects, and confidence that the validator is
//! actually answering.

use std::path::Path;
use std::time::Duration;

use crate::{RpcClient, RpcClientConfig, RpcError};

/// Why a validator endpoint could not be reached.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The configured address did not resolve.
    #[error("cannot resolve validator address {address}: {source}")]
    Address {
        /// The address as configured.
        address: String,
        /// The underlying resolution failure.
        source: std::io::Error,
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

    /// The validator did not answer within the attempt budget.
    #[error("validator at {url} did not answer after {attempts} attempts: {last_error}")]
    Unreachable {
        /// The endpoint that was probed.
        url: String,
        /// How many attempts were made.
        attempts: u32,
        /// The failure from the final attempt.
        last_error: String,
    },
}

/// Reads the credentials a validator expects from the configured parts.
///
/// A cookie path wins over an explicit user/password pair: a validator
/// configured for cookie auth will reject the pair. The cookie file's
/// `__cookie__:` prefix is stripped when present and tolerated when absent,
/// which is how older validators and some packagers write it.
pub fn auth_from_parts(
    cookie_path: Option<&Path>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Option<(String, String)>, ProbeError> {
    match cookie_path {
        Some(path) => {
            let contents = std::fs::read_to_string(path).map_err(|source| ProbeError::Cookie {
                path: path.display().to_string(),
                source,
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

/// How many times [`probe_node`] asks before giving up.
const PROBE_ATTEMPTS: u32 = 6;

/// Delay between probe attempts.
const PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// Waits for the validator at `address` to answer, and returns its URL.
///
/// A validator started alongside Zaino is not answering yet, so this retries
/// rather than failing on the first refusal. `getinfo` is the probe: every
/// supported validator implements it, and a successful response proves both
/// reachability and that the credentials are accepted.
///
/// # Failure is returned, not fatal
///
/// The version of this that lived in `zaino-fetch` called
/// `std::process::exit(1)` when the budget ran out. That made it unusable from
/// a test — it would take the test process with it — and unrecoverable for any
/// caller that might want to retry or report. It returns an error now; the one
/// production caller already propagated with `?`.
pub async fn probe_node(
    address: &str,
    cookie_path: Option<&Path>,
    user: Option<String>,
    password: Option<String>,
) -> Result<String, ProbeError> {
    let socket_addr =
        zaino_common::net::resolve_socket_addr(address).map_err(|source| ProbeError::Address {
            address: address.to_string(),
            source,
        })?;

    // An IPv6 literal needs bracketing before it can go in a URL.
    let host = match socket_addr {
        std::net::SocketAddr::V4(_) => socket_addr.ip().to_string(),
        std::net::SocketAddr::V6(_) => format!("[{}]", socket_addr.ip()),
    };
    let url = format!("http://{}:{}", host, socket_addr.port());

    let client = RpcClient::new(RpcClientConfig {
        url: url.clone(),
        auth: auth_from_parts(cookie_path, user, password)?,
        ..Default::default()
    })
    .map_err(ProbeError::Client)?;

    let mut last_error = String::new();
    for attempt in 0..PROBE_ATTEMPTS {
        match client.call("getinfo", Vec::new()).await {
            Ok(_) => return Ok(url),
            Err(error) => {
                last_error = error.to_string();
                if attempt + 1 < PROBE_ATTEMPTS {
                    tokio::time::sleep(PROBE_INTERVAL).await;
                }
            }
        }
    }

    Err(ProbeError::Unreachable {
        url,
        attempts: PROBE_ATTEMPTS,
        last_error,
    })
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

        assert_eq!(
            auth_from_parts(Some(file.path()), None, None).expect("cookie reads"),
            Some(("__cookie__".to_string(), "sekrit".to_string()))
        );
    }

    /// Some packagers write the token without the prefix. Treating that as part
    /// of the token would send the wrong password and fail authentication.
    #[test]
    fn a_bare_cookie_token_is_accepted() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        writeln!(file, "  sekrit  ").expect("write cookie");

        assert_eq!(
            auth_from_parts(Some(file.path()), None, None).expect("cookie reads"),
            Some(("__cookie__".to_string(), "sekrit".to_string()))
        );
    }

    /// A cookie path wins over an explicit pair: a validator configured for
    /// cookie auth rejects the pair, so preferring it would fail every request.
    #[test]
    fn a_cookie_path_wins_over_an_explicit_pair() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        write!(file, "__cookie__:sekrit").expect("write cookie");

        let auth = auth_from_parts(
            Some(file.path()),
            Some("user".to_string()),
            Some("pass".to_string()),
        )
        .expect("cookie reads");

        assert_eq!(auth, Some(("__cookie__".to_string(), "sekrit".to_string())));
    }

    #[test]
    fn an_explicit_pair_is_used_when_there_is_no_cookie() {
        assert_eq!(
            auth_from_parts(None, Some("user".to_string()), Some("pass".to_string()))
                .expect("no file to read"),
            Some(("user".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn a_missing_cookie_file_is_reported() {
        assert!(matches!(
            auth_from_parts(Some(Path::new("/nonexistent/cookie")), None, None),
            Err(ProbeError::Cookie { .. })
        ));
    }

    #[tokio::test]
    async fn an_unresolvable_address_is_reported() {
        assert!(matches!(
            probe_node("not a socket address", None, None, None).await,
            Err(ProbeError::Address { .. })
        ));
    }
}
