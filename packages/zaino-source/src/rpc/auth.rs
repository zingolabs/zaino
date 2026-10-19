//! Validator JSON-RPC basic-auth credentials from config (cookie file or user/password pair)

use std::path::Path;

use crate::EndpointError;

/// Basic-auth credentials a validator expects, from the configured parts (`None` = no auth)
///
/// - cookie path wins over a user/password pair (a cookie-auth validator rejects the pair)
/// - `__cookie__:` prefix stripped if present (older validators, some packagers omit it)
pub(crate) fn auth_from_parts(
    cookie_path: Option<&Path>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Option<(String, String)>, EndpointError> {
    let Some(path) = cookie_path else {
        return match (user, password) {
            (Some(user), Some(password)) => Ok(Some((user, password))),
            (None, None) => Ok(None),
            _ => Err(EndpointError::UnpairedCredentials),
        };
    };
    let contents = std::fs::read_to_string(path)
        .map_err(|source| EndpointError::Cookie { path: path.display().to_string(), source })?;
    let token = contents.trim();
    let token = token.strip_prefix("__cookie__:").unwrap_or(token);
    Ok(Some(("__cookie__".to_string(), token.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// - Cookie wins over a configured pair, user always `__cookie__`
    /// - Token trimmed with or without its prefix (packagers write both)
    /// - No cookie = the pair, none, or a half pair refused
    /// - Unreadable cookie = error
    #[test]
    fn credentials_come_from_the_cookie_first_then_the_pair() {
        let cookie = |contents: &str| {
            let mut file = tempfile::NamedTempFile::new().expect("temp file");
            write!(file, "{contents}").expect("write cookie");
            file
        };
        let pair = || (Some("user".to_owned()), Some("pass".to_owned()));
        let token = Some(("__cookie__".to_owned(), "sekrit".to_owned()));

        for contents in ["__cookie__:sekrit", "  sekrit  \n"] {
            let file = cookie(contents);
            let (user, pass) = pair();
            let auth = auth_from_parts(Some(file.path()), user, pass).expect("cookie reads");
            assert_eq!(auth, token, "{contents:?}");
        }
        let (user, pass) = pair();
        let explicit = auth_from_parts(None, user, pass).expect("no file to read");
        assert_eq!(explicit, Some(("user".to_owned(), "pass".to_owned())));
        assert_eq!(auth_from_parts(None, None, None).expect("no file to read"), None);
        let half = auth_from_parts(None, Some("user".to_owned()), None);
        assert!(matches!(half, Err(EndpointError::UnpairedCredentials)));
        let missing = auth_from_parts(Some(Path::new("/nonexistent/cookie")), None, None);
        assert!(matches!(missing, Err(EndpointError::Cookie { .. })));
    }
}
