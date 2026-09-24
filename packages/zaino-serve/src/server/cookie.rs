use std::{
    fs::{remove_file, File},
    io::{self, Write},
    path::Path,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore as _;
use subtle::ConstantTimeEq as _;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;

/// The name of the cookie file inside the cookie directory.
const FILE: &str = ".cookie";

/// The secret every request must present when cookie authentication is enabled.
#[derive(Clone, Debug)]
pub struct Cookie(String);

impl Cookie {
    /// Compares `passwd` with the cookie in constant time, so the comparison leaks no timing.
    pub fn authenticate(&self, passwd: String) -> bool {
        if passwd.len() != self.0.len() {
            return false;
        }
        passwd.as_bytes().ct_eq(self.0.as_bytes()).into()
    }
}

#[cfg(test)]
impl Cookie {
    /// A cookie with a secret the test chose.
    pub(crate) fn from_secret(secret: String) -> Self {
        Self(secret)
    }
}

impl Default for Cookie {
    fn default() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(STANDARD.encode(bytes))
    }
}

/// Writes `cookie` to `dir` as `__cookie__:<secret>`, readable only by the owner, refusing a symlinked path.
pub fn write_to_disk(cookie: &Cookie, dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;

    let cookie_path = dir.join(FILE);
    if cookie_path
        .symlink_metadata()
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(io::Error::other(format!(
            "cookie path {cookie_path:?} is a symlink, refusing to write"
        )));
    }

    let mut file = create_owner_only_file(&cookie_path)?;
    file.write_all(format!("__cookie__:{}", cookie.0).as_bytes())?;

    tracing::info!("RPC auth cookie written to disk");
    Ok(())
}

/// Creates or truncates `path` so that only its owner can read or write it.
fn create_owner_only_file(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    options.mode(0o600);

    options.open(path)
}

/// Removes the cookie file from `dir`.
pub fn remove_from_disk(dir: &Path) -> io::Result<()> {
    remove_file(dir.join(FILE))?;

    tracing::info!("RPC auth cookie removed from disk");
    Ok(())
}

#[cfg(test)]
mod write_to_disk {
    use super::*;

    #[test]
    fn is_written_in_the_zcash_cli_format_and_authenticates_only_its_secret() {
        let dir = tempfile::tempdir().expect("a temporary directory is creatable");
        let cookie = Cookie::default();
        write_to_disk(&cookie, dir.path()).expect("the cookie writes");

        let written = std::fs::read_to_string(dir.path().join(FILE)).expect("the cookie reads");
        let secret = written
            .strip_prefix("__cookie__:")
            .expect("the file carries the zcash-cli user name");
        assert!(cookie.authenticate(secret.to_string()));
        assert!(!cookie.authenticate(format!("{secret}x")));
        assert!(!cookie.authenticate(String::new()));

        remove_from_disk(dir.path()).expect("the cookie removes");
        assert!(!dir.path().join(FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temporary directory is creatable");
        write_to_disk(&Cookie::default(), dir.path()).expect("the cookie writes");

        let mode = std::fs::metadata(dir.path().join(FILE))
            .expect("the cookie exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_symlink() {
        let dir = tempfile::tempdir().expect("a temporary directory is creatable");
        let target = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, dir.path().join(FILE)).expect("the symlink creates");

        assert!(write_to_disk(&Cookie::default(), dir.path()).is_err());
        assert!(!target.exists());
    }
}
