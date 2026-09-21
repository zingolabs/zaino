//! The zainod daemon configuration.
//!
//! This is the **daemon-mode** config surface. The runtime stack crates
//! ([`zaino_runtime`], [`zaino_indexer`], [`zaino_store`], [`zaino_lightserve`],
//! the source adapters) are deliberately config-agnostic — they take typed
//! params. This module is where operator config comes through, and
//! [`crate::indexer::spawn_indexer`] translates it into those typed params at
//! boot. The wallet API will get its own, separate config; keeping this one
//! self-contained keeps that boundary clean.
//!
//! It carries only what the runtime serving stack consumes. Config is layered
//! highest-priority-first:
//! environment variables (`ZAINO_` prefix, `__` nesting), then the TOML file,
//! then built-in defaults.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use zaino_indexer::FetchConcurrency;

use serde::{Deserialize, Serialize};
use tracing::info;

use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;

use crate::error::IndexerError;

/// Header prepended to a generated configuration file.
pub const GENERATED_CONFIG_HEADER: &str = r#"# Zaino daemon configuration
#
# Generated with `zainod generate-config`.
#
# Layered highest-priority-first: ZAINO_ env vars, then this file, then defaults.
# For documentation see https://github.com/zingolabs/zaino
"#;

/// The validator's Zebra JSON-RPC endpoint, the daemon's only block source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SourceConfig {
    /// The validator's JSON-RPC listen address (`host:port`).
    pub jsonrpc_address: String,
    /// Path to the validator's auth cookie, if it uses cookie auth.
    pub cookie_path: Option<PathBuf>,
    /// JSON-RPC basic-auth user, if configured.
    pub user: Option<String>,
    /// JSON-RPC basic-auth password, if configured.
    pub password: Option<String>,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            jsonrpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
        }
    }
}

/// The finalised index store (LMDB).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreConfig {
    /// Directory holding the LMDB environment (created if absent).
    pub path: PathBuf,
    /// Maximum on-disk size in GiB, reserved up front (LMDB requires a map size
    /// bound at open time).
    pub map_size_gb: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: crate::paths::default_store(),
            map_size_gb: 16,
        }
    }
}

/// The wallet-facing gRPC server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServeConfig {
    /// Address the `CompactTxStreamer` gRPC server listens on.
    pub grpc_listen_address: SocketAddr,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            grpc_listen_address: "127.0.0.1:8137".parse().expect("valid default addr"),
        }
    }
}

/// Index-build tuning.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexerConfig {
    /// Blocks committed per atomic batch.
    pub batch_size: u32,
    /// Contexts buffered between the provisioner and the engine.
    pub channel_capacity: usize,
    /// Depth below the tip treated as still volatile; only `tip − depth` and
    /// below is indexed.
    pub finalised_depth: u32,
    /// Fetches kept in flight by the provisioner. Concurrent fetch keeps the
    /// parallel engine fed rather than paced by a one-at-a-time loop. A
    /// `concurrency = 0` in the config is rejected at parse time (non-zero type).
    pub concurrency: FetchConcurrency,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            channel_capacity: 256,
            finalised_depth: MAX_BLOCK_REORG_HEIGHT,
            concurrency: FetchConcurrency::new(NonZeroUsize::new(16).expect("16 is non-zero")),
        }
    }
}

/// The zainod daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// Prometheus `/metrics` endpoint. Disabled when absent; requires the
    /// `prometheus` feature.
    pub metrics_endpoint: Option<SocketAddr>,
    /// The validator blocks are sourced from.
    pub source: SourceConfig,
    /// The finalised index store.
    pub store: StoreConfig,
    /// The wallet-facing gRPC server.
    pub serve: ServeConfig,
    /// Index-build tuning.
    pub indexer: IndexerConfig,
}

/// The env var that activates the ztest regtest fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_ENV: &str = "ZAINO_TEST_REGTEST_FIXTURE";

/// TEST-ONLY: an in-process config for the ztest regtest e2e.
///
/// ztest mounts a *legacy*-schema `zainod.toml` this config cannot parse (and injects no
/// `ZAINO_` env), so the e2e sets [`TEST_FIXTURE_ENV`] and zainod boots this hardcoded config
/// instead — ignoring the mounted `--config` — matching ztest's container paths/ports.
///
/// NEVER for production: gated behind BOTH the `ztest-fixture` build feature and
/// the runtime env var, and its activation logs a loud warning.
#[cfg(feature = "ztest-fixture")]
pub fn regtest_fixture() -> DaemonConfig {
    // Validator = separate pod in the ztest cluster; local regtest default when unset
    let jsonrpc_address =
        std::env::var(TEST_FIXTURE_JSONRPC_ENV).unwrap_or_else(|_| "127.0.0.1:18232".to_string());
    DaemonConfig {
        metrics_endpoint: None,
        source: SourceConfig {
            jsonrpc_address,
            ..SourceConfig::default()
        },
        store: StoreConfig {
            path: PathBuf::from("/var/lib/zaino/db"),
            map_size_gb: 4,
        },
        serve: ServeConfig {
            grpc_listen_address: "0.0.0.0:8137".parse().expect("valid fixture addr"),
        },
        indexer: IndexerConfig {
            // A regtest chain is a handful of blocks; index right to the tip
            // (no reorg margin) so the mined blocks are actually served.
            finalised_depth: 0,
            ..IndexerConfig::default()
        },
    }
}

/// Env var the e2e uses to hand the fixture the validator's in-cluster JSON-RPC
/// address (`host:port`) (see [`regtest_fixture`]).
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_JSONRPC_ENV: &str = "ZAINO_TEST_ZEBRA_JSONRPC";

/// Serialize the built-in defaults into a commented example config file.
pub fn generate_default_config() -> Result<String, IndexerError> {
    let toml = toml::to_string_pretty(&DaemonConfig::default())
        .map_err(|e| IndexerError::ConfigError(format!("serialising default config: {e}")))?;
    Ok(format!("{GENERATED_CONFIG_HEADER}{toml}"))
}

/// Load configuration from a TOML file with `ZAINO_` environment overrides.
pub fn load_config(file_path: &std::path::Path) -> Result<DaemonConfig, IndexerError> {
    load_config_with_env(file_path, "ZAINO")
}

/// Load configuration with a custom environment-variable prefix.
///
/// Layering: defaults → TOML file → environment (`<prefix>_`, `__` for nesting).
pub fn load_config_with_env(
    file_path: &std::path::Path,
    env_prefix: &str,
) -> Result<DaemonConfig, IndexerError> {
    let settings = config::Config::builder()
        .add_source(
            config::File::from(file_path)
                .format(config::FileFormat::Toml)
                .required(true),
        )
        .add_source(
            config::Environment::with_prefix(env_prefix)
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()
        .map_err(|e| IndexerError::ConfigError(format!("loading configuration: {e}")))?;

    let parsed: DaemonConfig = settings
        .try_deserialize()
        .map_err(|e| IndexerError::ConfigError(format!("parsing configuration: {e}")))?;

    info!(path = %file_path.display(), "config loaded");
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).expect("write config");
        path
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let original = DaemonConfig::default();
        let toml = toml::to_string_pretty(&original).expect("serialise");
        let parsed: DaemonConfig = toml::from_str(&toml).expect("deserialise");
        assert_eq!(original, parsed);
    }

    #[test]
    fn generated_config_is_valid_toml_with_header() {
        let content = generate_default_config().expect("generate");
        assert!(content.starts_with(GENERATED_CONFIG_HEADER));
        let body = content
            .strip_prefix(GENERATED_CONFIG_HEADER)
            .expect("header present");
        toml::from_str::<DaemonConfig>(body).expect("body parses");
    }

    #[test]
    fn source_parses_with_auth_absent_and_direct_era_fields_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
[source]
jsonrpc_address = "127.0.0.1:18232"

[store]
path = "/tmp/zaino-store"
"#;
        let config = load_config(&write(&dir, "rpc.toml", toml)).expect("load");
        assert_eq!(
            config.source,
            SourceConfig {
                jsonrpc_address: "127.0.0.1:18232".to_string(),
                cookie_path: None,
                user: None,
                password: None,
            }
        );
        assert_eq!(config.indexer, IndexerConfig::default());

        for (name, stale_line) in [
            ("mode.toml", r#"mode = "direct""#),
            ("cache.toml", r#"zebra_cache_dir = "/var/lib/zebra""#),
        ] {
            let stale = toml.replace("[source]\n", &format!("[source]\n{stale_line}\n"));
            let err = load_config(&write(&dir, name, &stale)).expect_err(stale_line);
            assert!(
                err.to_string().contains("unknown field"),
                "{stale_line}: {err}"
            );
        }
    }

    #[test]
    fn env_overrides_a_scalar_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
[source]
jsonrpc_address = "127.0.0.1:8232"

[store]
path = "/tmp/zaino-store"
"#;
        let path = write(&dir, "env.toml", toml);
        // A leaf scalar override applies over the file value. nextest runs each
        // test in its own process, so this env var does not leak across tests.
        std::env::set_var("ZAINO_INDEXER__BATCH_SIZE", "42");
        let config = load_config(&path).expect("load");
        std::env::remove_var("ZAINO_INDEXER__BATCH_SIZE");
        assert_eq!(config.indexer.batch_size, 42);
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
bogus_field = true

[store]
path = "/tmp/zaino-store"
"#;
        let path = write(&dir, "bogus.toml", toml);
        assert!(load_config(&path).is_err());
    }
}
