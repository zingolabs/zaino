//! XDG Base Directory utilities for consistent path resolution.
//!
//! This module provides a centralized policy for resolving default paths
//! following the [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/).
//!
//! # Resolution Policy
//!
//! Paths are resolved in the following order:
//! 1. Check the XDG environment variable (e.g., `XDG_CONFIG_HOME`)
//! 2. Fall back to `$HOME/{subdir}` (e.g., `$HOME/.config`)
//! 3. Fall back to `/tmp/zaino/{subdir}` if HOME is not set
//!
//! # Example
//!
//! ```
//! use zaino_common::xdg::{resolve_path_with_xdg_cache_defaults, resolve_path_with_xdg_config_defaults};
//!
//! // Resolves to $XDG_CONFIG_HOME/zaino/zainod.toml, or ~/.config/zaino/zainod.toml
//! let config_path = resolve_path_with_xdg_config_defaults("zaino/zainod.toml");
//!
//! // Resolves to $XDG_CACHE_HOME/zaino, or ~/.cache/zaino
//! let cache_path = resolve_path_with_xdg_cache_defaults("zaino");
//! ```

use std::path::PathBuf;

/// XDG Base Directory categories.
///
/// Each variant corresponds to an XDG environment variable and its
/// standard fallback location relative to `$HOME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdgDir {
    /// `XDG_CONFIG_HOME` - User configuration files.
    ///
    /// Default: `$HOME/.config`
    Config,

    /// `XDG_CACHE_HOME` - Non-essential cached data.
    ///
    /// Default: `$HOME/.cache`
    Cache,
    // /// `XDG_DATA_HOME` - User data files.
    // ///
    // /// Default: `$HOME/.local/share`
    // Data,

    // /// `XDG_STATE_HOME` - Persistent state (logs, history).
    // ///
    // /// Default: `$HOME/.local/state`
    // State,

    // /// `XDG_RUNTIME_DIR` - Runtime files (sockets, locks).
    // ///
    // /// No standard default; falls back to `/tmp` if unset.
    // Runtime,
}

impl XdgDir {
    /// Returns the environment variable name for this XDG directory.
    pub fn env_var(&self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Cache => "XDG_CACHE_HOME",
        }
    }

    /// Returns the fallback subdirectory relative to `$HOME`.
    pub fn home_subdir(&self) -> &'static str {
        match self {
            Self::Config => ".config",
            Self::Cache => ".cache",
        }
    }
}

/// Resolves a path using XDG Base Directory defaults.
///
/// # Resolution Order
///
/// 1. If the XDG environment variable is set, uses that as the base
/// 2. Falls back to `$HOME/{xdg_subdir}/{subpath}`
/// 3. Falls back to `/tmp/zaino/{xdg_subdir}/{subpath}` if HOME is unset
fn resolve_path_with_xdg_defaults(dir: XdgDir, subpath: &str) -> PathBuf {
    // Try XDG environment variable first
    if let Ok(xdg_base) = std::env::var(dir.env_var()) {
        return PathBuf::from(xdg_base).join(subpath);
    }

    // Fall back to $HOME/{subdir}
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(dir.home_subdir()).join(subpath);
    }

    // Final fallback to /tmp/zaino/{subdir}
    PathBuf::from("/tmp")
        .join("zaino")
        .join(dir.home_subdir())
        .join(subpath)
}

/// Resolves a path using `XDG_CONFIG_HOME` defaults.
///
/// Convenience wrapper for [`resolve_path_with_xdg_defaults`] with [`XdgDir::Config`].
///
/// # Example
///
/// ```
/// use zaino_common::xdg::resolve_path_with_xdg_config_defaults;
///
/// let path = resolve_path_with_xdg_config_defaults("zaino/zainod.toml");
/// // Returns: $XDG_CONFIG_HOME/zaino/zainod.toml
/// //      or: $HOME/.config/zaino/zainod.toml
/// //      or: /tmp/zaino/.config/zaino/zainod.toml
/// ```
pub fn resolve_path_with_xdg_config_defaults(subpath: &str) -> PathBuf {
    resolve_path_with_xdg_defaults(XdgDir::Config, subpath)
}

/// Resolves a path using `XDG_CACHE_HOME` defaults.
///
/// Convenience wrapper for [`resolve_path_with_xdg_defaults`] with [`XdgDir::Cache`].
///
/// # Example
///
/// ```
/// use zaino_common::xdg::resolve_path_with_xdg_cache_defaults;
///
/// let path = resolve_path_with_xdg_cache_defaults("zaino");
/// // Returns: $XDG_CACHE_HOME/zaino
/// //      or: $HOME/.cache/zaino
/// //      or: /tmp/zaino/.cache/zaino
/// ```
pub fn resolve_path_with_xdg_cache_defaults(subpath: &str) -> PathBuf {
    resolve_path_with_xdg_defaults(XdgDir::Cache, subpath)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xdg_dir_env_vars() {
        assert_eq!(XdgDir::Config.env_var(), "XDG_CONFIG_HOME");
        assert_eq!(XdgDir::Cache.env_var(), "XDG_CACHE_HOME");
    }

    #[test]
    fn test_xdg_dir_home_subdirs() {
        assert_eq!(XdgDir::Config.home_subdir(), ".config");
        assert_eq!(XdgDir::Cache.home_subdir(), ".cache");
    }

    #[test]
    fn test_resolved_paths_end_with_subpath() {
        let config_path = resolve_path_with_xdg_config_defaults("zaino/zainod.toml");
        assert!(config_path.ends_with("zaino/zainod.toml"));

        let cache_path = resolve_path_with_xdg_cache_defaults("zaino");
        assert!(cache_path.ends_with("zaino"));
    }
}
