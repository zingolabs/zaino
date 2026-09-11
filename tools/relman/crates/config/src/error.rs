use std::path::PathBuf;

use relman_core::types::{InvalidCrateName, InvalidWorkspacePath};

/// Everything that can go wrong loading a `relman.toml`.
///
/// Parse-don't-validate: invalid input fails here, at load, so downstream
/// holders of a [`ReleaseConfig`](crate::ReleaseConfig) never re-check.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The manifest file could not be read.
    #[error("failed to read config file {path}")]
    Io {
        /// The path relman tried to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file was not valid TOML for the expected schema.
    #[error("failed to parse TOML config {path}")]
    Toml {
        /// The path being parsed.
        path: PathBuf,
        /// The underlying deserialization error.
        #[source]
        source: toml::de::Error,
    },
    /// A target's `name` was not a valid crate name.
    #[error("invalid target crate name {name:?}")]
    InvalidCrateName {
        /// The rejected raw string.
        name: String,
        /// Why it was rejected.
        #[source]
        source: InvalidCrateName,
    },
    /// A path field (option or target) was not a valid workspace path.
    #[error("invalid path {value:?} for {field}")]
    InvalidPath {
        /// Which field carried the bad path (for diagnostics).
        field: String,
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidWorkspacePath,
    },
    /// Two `[[target]]` entries declared the same name.
    #[error("duplicate target name {0:?}")]
    DuplicateTarget(String),
    /// The manifest declared no targets; at least one is required.
    #[error("relman.toml must declare at least one [[target]]")]
    NoTargets,
}
