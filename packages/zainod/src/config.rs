//! The zainod daemon configuration.
//!
//! This is the **daemon-mode** config surface. The runtime stack crates
//! ([`zaino_runtime`], [`zaino_lightserve`], the source adapters) are
//! deliberately config-agnostic — they take typed params, and the runtime's
//! own sections ([`StoreConfig`], [`IndexerConfig`]) are re-exported here. This module is where operator config comes through, and
//! [`crate::indexer::spawn_indexer`] translates it into those typed params at
//! boot. The wallet API will get its own, separate config; keeping this one
//! self-contained keeps that boundary clean.
//!
//! Greenfield, not the legacy `zaino-state` config: it carries only what the
//! runtime serving stack consumes. Config is layered highest-priority-first:
//! environment variables (`ZAINO_` prefix, `__` nesting), then the TOML file,
//! then built-in defaults.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::info;

pub use zaino_common::Network;
pub use zaino_runtime::config::{FetchStrategy, IndexerConfig, StoreConfig};

use crate::error::IndexerError;

/// Header prepended to a generated configuration file.
pub const GENERATED_CONFIG_HEADER: &str = r#"# Zaino daemon configuration
#
# Generated with `zainod generate-config`.
#
# Layered highest-priority-first: ZAINO_ env vars, then this file, then defaults.
# For documentation see https://github.com/zingolabs/zaino
"#;

/// Where the daemon sources blocks from the validator.
///
/// `Rpc` talks JSON-RPC (works off-node, no state DB) and follows the live tip:
/// the chain-head polls the tip over the same transport. It is the only source
/// mode.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum SourceMode {
    /// Zebra JSON-RPC.
    Rpc {
        /// The validator's JSON-RPC listen address (`host:port`).
        jsonrpc_address: String,
        /// Path to the validator's auth cookie, if it uses cookie auth.
        #[serde(default)]
        cookie_path: Option<PathBuf>,
        /// JSON-RPC basic-auth user, if configured.
        #[serde(default)]
        user: Option<String>,
        /// JSON-RPC basic-auth password, if configured.
        #[serde(default)]
        password: Option<String>,
    },
}

impl Default for SourceMode {
    fn default() -> Self {
        // Off-node RPC to a local validator is the least-assuming default; a
        // co-located deployment opts into `Direct`.
        SourceMode::Rpc {
            jsonrpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
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

/// Which deployment to run.
///
/// A closed set: each variant is a static deployment
/// (`zaino_runtime::deployment`) that binds a use case, a routing and an
/// index set, checked by the compiler. Config selects one; it does not shape
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DeploymentKind {
    /// The light-wallet use case over the compact-block index set, with
    /// everything the wallet parses itself relayed to the validator.
    #[default]
    LightWalletPassthrough,
}

/// The zainod daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// Which deployment to run.
    pub deployment: DeploymentKind,
    /// Network the validator serves.
    pub network: Network,
    /// Prometheus `/metrics` endpoint. Disabled when absent; requires the
    /// `prometheus` feature.
    pub metrics_endpoint: Option<SocketAddr>,
    /// Where blocks are sourced from.
    pub source: SourceMode,
    /// The finalised index store.
    pub store: StoreConfig,
    /// The wallet-facing gRPC server.
    pub serve: ServeConfig,
    /// Index-build tuning.
    pub indexer: IndexerConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            deployment: DeploymentKind::default(),
            network: Network::Mainnet,
            metrics_endpoint: None,
            source: SourceMode::default(),
            store: StoreConfig {
                path: zaino_common::xdg::resolve_path_with_xdg_cache_defaults("zaino/store"),
                map_size_gb: StoreConfig::default_map_size_gb(),
            },
            serve: ServeConfig::default(),
            indexer: IndexerConfig::default(),
        }
    }
}

impl DaemonConfig {
    /// Validate cross-field invariants that parsing alone cannot.
    ///
    /// The typed fields (socket addresses, the tagged source mode) already carry
    /// their own validation, so there are currently no further cross-field
    /// invariants to check. Kept as the extension point config validation hangs
    /// off, and as the stable call site in [`load_config_with_env`].
    pub fn validate(&self) -> Result<(), IndexerError> {
        Ok(())
    }
}

/// Env var handing the mainnet Rpc fixture the validator's JSON-RPC address
/// (`host:port`), which the source dials for blocks, tip, mempool and
/// passthrough (see [`mainnet_rpc_fixture`]).
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_JSONRPC_ENV: &str = "ZAINO_TEST_ZEBRA_JSONRPC";

/// Env var handing the mainnet Rpc fixture the writable FS-store directory
/// (see [`mainnet_rpc_fixture`]). Optional; defaults to a cache path.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_STORE_ENV: &str = "ZAINO_TEST_STORE_DIR";

/// Env var handing the mainnet Rpc fixture the LMDB map size in GiB — the
/// reserved store ceiling (see [`mainnet_rpc_fixture`]). Optional;
/// defaults to a mainnet-safe value. Tune it per deploy without a rebuild, and
/// keep it below the backing volume's capacity.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_MAP_SIZE_ENV: &str = "ZAINO_TEST_MAP_SIZE_GB";

/// Env var selecting what the mainnet Rpc fixture's indexer fetches per height:
/// `compact` (the fork's pre-index compact block, the default) or `full` (whole
/// blocks over the standard read, which any validator answers — see
/// [`FetchStrategy`]). Optional; an unrecognised value is reported and the
/// default kept.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_FETCH_ENV: &str = "ZAINO_TEST_FETCH";

/// The fetch strategy the mainnet Rpc fixture's env selects, default when unset.
#[cfg(feature = "ztest-fixture")]
fn fixture_fetch_strategy() -> FetchStrategy {
    match std::env::var(TEST_FIXTURE_FETCH_ENV).as_deref() {
        Ok("compact") => FetchStrategy::Compact,
        Ok("full") => FetchStrategy::Full,
        Ok(other) => {
            tracing::warn!(
                env = TEST_FIXTURE_FETCH_ENV,
                value = other,
                "unrecognised fetch strategy; expected `compact` or `full`, keeping the default"
            );
            FetchStrategy::default()
        }
        Err(_) => FetchStrategy::default(),
    }
}

/// Default LMDB map size (GiB) for the mainnet Rpc fixture. A full mainnet
/// index far exceeds a regtest chain's, so this is generous headroom over what
/// the index actually occupies; the deploy caps it under the volume size.
#[cfg(feature = "ztest-fixture")]
const MAINNET_FIXTURE_MAP_SIZE_GB: usize = 64;

/// The env var that activates the mainnet **Rpc** fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
///
/// It boots the greenfield daemon against the validator's JSON-RPC (the only
/// source), so the daemon can run under a real deploy whose chart still mounts a
/// legacy-schema `zainod.toml` this loader cannot parse.
#[cfg(feature = "ztest-fixture")]
pub const MAINNET_RPC_FIXTURE_ENV: &str = "ZAINO_MAINNET_RPC_FIXTURE";

/// TEST/DEPLOY-ONLY: a mainnet **Rpc** [`DaemonConfig`] built entirely from
/// env-supplied topology, for booting the greenfield daemon against the
/// validator's JSON-RPC. The store path defaults so an Rpc-mode deploy can reuse
/// an existing index PVC (the index is source-agnostic) rather than reindexing.
///
/// Topology arrives by env so one image serves any cluster:
/// - [`TEST_FIXTURE_JSONRPC_ENV`]: the validator JSON-RPC `host:port`.
/// - [`TEST_FIXTURE_STORE_ENV`]: the writable FS-store directory.
/// - [`TEST_FIXTURE_MAP_SIZE_ENV`]: the LMDB map size in GiB (store ceiling).
/// - [`TEST_FIXTURE_FETCH_ENV`]: `compact` or `full` — what the indexer fetches
///   per height. `full` lets the fixture index from a stock validator.
///
/// The serving policy (mainnet, reorg-margin finalised depth, gRPC on
/// `0.0.0.0:8137`) is baked. NEVER for production: gated behind BOTH the
/// `ztest-fixture` build feature and the runtime env var, with a loud warning on
/// activation.
#[cfg(feature = "ztest-fixture")]
pub fn mainnet_rpc_fixture() -> DaemonConfig {
    let jsonrpc_address = std::env::var(TEST_FIXTURE_JSONRPC_ENV)
        .unwrap_or_else(|_| "zebra.golden-zebra-state.svc:8232".to_string());
    let store_path = std::env::var_os(TEST_FIXTURE_STORE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/zaino/.cache/zaino/store"));
    let map_size_gb = std::env::var(TEST_FIXTURE_MAP_SIZE_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(MAINNET_FIXTURE_MAP_SIZE_GB);
    DaemonConfig {
        deployment: DeploymentKind::default(),
        network: Network::Mainnet,
        metrics_endpoint: None,
        source: SourceMode::Rpc {
            jsonrpc_address,
            cookie_path: None,
            user: None,
            password: None,
        },
        store: StoreConfig {
            path: store_path,
            map_size_gb,
        },
        serve: ServeConfig {
            grpc_listen_address: "0.0.0.0:8137".parse().expect("valid fixture addr"),
        },
        indexer: IndexerConfig {
            fetch: fixture_fetch_strategy(),
            ..IndexerConfig::default()
        },
    }
}

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

    parsed.validate()?;
    info!(path = %file_path.display(), "config loaded and validated");
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
    fn rpc_source_parses_with_optional_auth_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
network = "Regtest"

[source]
mode = "rpc"
jsonrpc_address = "127.0.0.1:18232"

[store]
path = "/tmp/zaino-store"
"#;
        let path = write(&dir, "rpc.toml", toml);
        let config = load_config(&path).expect("load");
        match config.source {
            SourceMode::Rpc {
                jsonrpc_address,
                cookie_path,
                ..
            } => {
                assert_eq!(jsonrpc_address, "127.0.0.1:18232");
                assert!(cookie_path.is_none());
            }
            other => panic!("expected Rpc source, got {other:?}"),
        }
        // Untouched sections keep their defaults.
        assert_eq!(config.indexer, IndexerConfig::default());
    }

    #[test]
    fn env_overrides_a_scalar_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
network = "Mainnet"

[source]
mode = "rpc"
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
network = "Mainnet"
bogus_field = true

[store]
path = "/tmp/zaino-store"
"#;
        let path = write(&dir, "bogus.toml", toml);
        assert!(load_config(&path).is_err());
    }

    /// The mainnet Rpc fixture is a mainnet, Rpc-source config with the baked
    /// serving policy and the default (unset) JSON-RPC endpoint. nextest runs each
    /// test in its own process, so the endpoint env var does not leak.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn mainnet_rpc_fixture_is_a_mainnet_rpc_config() {
        std::env::remove_var(super::TEST_FIXTURE_JSONRPC_ENV);
        let config = super::mainnet_rpc_fixture();
        assert_eq!(config.network, Network::Mainnet);
        match config.source {
            SourceMode::Rpc {
                jsonrpc_address,
                cookie_path,
                user,
                password,
            } => {
                assert_eq!(jsonrpc_address, "zebra.golden-zebra-state.svc:8232");
                assert!(cookie_path.is_none() && user.is_none() && password.is_none());
            }
            other => panic!("expected Rpc source, got {other:?}"),
        }
        // Serving policy is baked; the default store path is the one an Rpc-mode
        // deploy reuses its existing index PVC at.
        assert_eq!(
            config.serve.grpc_listen_address,
            "0.0.0.0:8137".parse().expect("valid addr"),
        );
        assert_eq!(
            config.store.path,
            PathBuf::from("/home/zaino/.cache/zaino/store")
        );
    }

    /// The endpoint env override reaches the Rpc fixture.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn mainnet_rpc_fixture_takes_the_endpoint_env() {
        std::env::set_var(super::TEST_FIXTURE_JSONRPC_ENV, "zebra.example.svc:8232");
        let config = super::mainnet_rpc_fixture();
        std::env::remove_var(super::TEST_FIXTURE_JSONRPC_ENV);
        match config.source {
            SourceMode::Rpc {
                jsonrpc_address, ..
            } => {
                assert_eq!(jsonrpc_address, "zebra.example.svc:8232")
            }
            other => panic!("expected Rpc source, got {other:?}"),
        }
    }
}
