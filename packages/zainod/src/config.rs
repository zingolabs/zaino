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
pub use zaino_runtime::config::{DeferralPolicy, FetchStrategy, IndexerConfig, StoreConfig};

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
/// `Direct` reads the validator's on-disk state database in-process (fastest;
/// must be co-located with the validator). `Rpc` talks JSON-RPC (works off-node,
/// no state DB). Both follow the live tip: the chain-head polls the tip over the
/// configured transport. `Rpc` trades the state DB's disk-speed reads for
/// per-block RPC round-trips.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum SourceMode {
    /// Direct Zebra `ReadState`: reads the on-disk state DB under `zebra_cache_dir`.
    ///
    /// The JSON-RPC coordinates are still required: the state database serves
    /// finalised blocks (the compact-serving fast path) but cannot answer the
    /// mempool or passthrough RPCs, which reach the validator no other way. The
    /// auth fields mirror [`SourceMode::Rpc`].
    Direct {
        /// Root of the validator's Zebra cache directory (the state DB lives
        /// under it, keyed by network).
        zebra_cache_dir: PathBuf,
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

/// The serving sockets.
///
/// Which one is used follows from the selected [`DeploymentKind`]: the
/// light-wallet deployment serves the `CompactTxStreamer` gRPC on
/// `grpc_listen_address`, and the node-RPC / explorer deployment serves the
/// Zcash JSON-RPC on `jsonrpc_listen_address`. Both default to loopback; a
/// deployment binds the one its protocol needs and leaves the other at its
/// (unused) default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServeConfig {
    /// Address the `CompactTxStreamer` gRPC server listens on (the light-wallet
    /// deployment).
    pub grpc_listen_address: SocketAddr,
    /// Address the Zcash node JSON-RPC server listens on (the node-RPC /
    /// explorer deployment). Defaults to the zcashd JSON-RPC port on loopback.
    ///
    /// Binding a non-loopback address is permitted: the greenfield daemon binds
    /// the configured address directly, exactly as the gRPC side does, because
    /// the default-secure TLS / public-bind posture is not yet ported for
    /// either server (the `allow_unencrypted_public_json_rpc_bind` /
    /// `no_tls_*` build features are compat stubs that gate no behaviour). An
    /// in-cluster deploy therefore binds `0.0.0.0` through
    /// `ZAINO_SERVE__JSONRPC_LISTEN_ADDRESS` with no feature build, relying on
    /// the cluster Service boundary rather than on a bind refusal this stack
    /// does not yet implement.
    pub jsonrpc_listen_address: SocketAddr,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            grpc_listen_address: "127.0.0.1:8137".parse().expect("valid default addr"),
            jsonrpc_listen_address: "127.0.0.1:8232".parse().expect("valid default addr"),
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
    /// everything the wallet parses itself — transparent address history
    /// included — relayed to the validator. Serves the `CompactTxStreamer` gRPC
    /// on `serve.grpc_listen_address`.
    #[default]
    LightWalletPassthrough,
    /// The light-wallet use case over the transparent-history index set, serving
    /// address history locally so the wallet's queried addresses are never
    /// disclosed to the validator. Serves the `CompactTxStreamer` gRPC on
    /// `serve.grpc_listen_address`.
    LightWalletLocal,
    /// The node-RPC / explorer use case over the transparent-history index set,
    /// serving transparent address history and spend lookups locally (which a
    /// plain-RPC validator cannot answer) and relaying full and verbose blocks,
    /// decoded transactions, the chain-info aggregate, the node-status reads and
    /// the mempool listing to the validator; transaction location withheld.
    /// Serves the Zcash JSON-RPC on `serve.jsonrpc_listen_address`.
    ///
    /// Also accepts the legacy value `node-rpc-passthrough`, which the live
    /// cluster deploy still sets, as an alias.
    #[serde(alias = "node-rpc-passthrough")]
    NodeRpcLocal,
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
                deferred_writes: DeferralPolicy::default(),
            },
            serve: ServeConfig::default(),
            indexer: IndexerConfig::default(),
        }
    }
}

impl DaemonConfig {
    /// Validate cross-field invariants that parsing alone cannot.
    ///
    /// Kept minimal: `Direct` needs an existing cache directory (a missing one
    /// is a misconfiguration worth naming at startup rather than a cryptic
    /// database-open failure later). Socket addresses are already typed, so
    /// they need no re-parsing here.
    pub fn validate(&self) -> Result<(), IndexerError> {
        if let SourceMode::Direct {
            zebra_cache_dir, ..
        } = &self.source
        {
            if !zebra_cache_dir.is_dir() {
                return Err(IndexerError::ConfigError(format!(
                    "source.mode = \"direct\" but zebra_cache_dir {} is not an existing directory",
                    zebra_cache_dir.display(),
                )));
            }
        }
        Ok(())
    }
}

/// Test/deploy-only config fixtures, gated behind the `ztest-fixture` feature.
///
/// The fixtures that build a whole [`DaemonConfig`] from env-supplied topology —
/// for booting the daemon where the surrounding deploy mounts a legacy-schema
/// config this loader cannot parse — live in [`fixture`]. Their public entry
/// points are re-exported so `crate::config::<name>` resolves as before.
#[cfg(feature = "ztest-fixture")]
mod fixture;

#[cfg(feature = "ztest-fixture")]
pub use fixture::{
    fixture_deferred_writes, fixture_deployment, mainnet_direct_state_fixture, mainnet_rpc_fixture,
    regtest_direct_fixture, FIXTURE_DEPLOYMENT_ENV, MAINNET_RPC_FIXTURE_ENV,
    MAINNET_STATE_FIXTURE_ENV, TEST_FIXTURE_DEFERRED_WRITES_ENV, TEST_FIXTURE_ENV,
    TEST_FIXTURE_FETCH_ENV, TEST_FIXTURE_JSONRPC_ENV, TEST_FIXTURE_MAP_SIZE_ENV,
    TEST_FIXTURE_STORE_ENV, TEST_FIXTURE_ZEBRA_ENV,
};

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

/// Serialises the config tests that read or mutate process-global env vars.
///
/// `load_config` layers the `ZAINO_*` environment over the file, and several
/// tests `set_var`/`remove_var` to exercise that layering. Env vars are
/// process-global, so under the default thread-based test runner a mutator in
/// one test races a loader in another (a `set_var` leaking into an unrelated
/// `load_config`) — including across the fixture submodule, whose own tests
/// mutate `ZAINO_TEST_*`. Every such test takes this one lock for its whole body,
/// so they run one at a time and the default `cargo test` passes without
/// `--test-threads=1`. Poisoning (a panicking test) is recovered with
/// `into_inner` so one failure does not cascade into spurious lock errors.
#[cfg(test)]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`ENV_LOCK`] for the current test, recovering from poisoning. Shared with
/// the fixture submodule's tests so neither races the other's env mutations.
#[cfg(test)]
pub(in crate::config) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    fn direct_source_parses() {
        let _env = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        // Direct requires an existing cache dir; point it at the tempdir itself.
        let toml = format!(
            r#"
network = "Mainnet"

[source]
mode = "direct"
zebra_cache_dir = "{}"
jsonrpc_address = "127.0.0.1:18232"

[store]
path = "/tmp/zaino-store"

[serve]
grpc_listen_address = "127.0.0.1:8137"
"#,
            dir.path().display(),
        );
        let path = write(&dir, "direct.toml", &toml);
        let config = load_config(&path).expect("load");
        match config.source {
            SourceMode::Direct {
                jsonrpc_address,
                cookie_path,
                ..
            } => {
                assert_eq!(jsonrpc_address, "127.0.0.1:18232");
                assert!(cookie_path.is_none());
            }
            other => panic!("expected Direct source, got {other:?}"),
        }
        assert_eq!(config.network, Network::Mainnet);
    }

    #[test]
    fn rpc_source_parses_with_optional_auth_absent() {
        let _env = lock_env();
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
        let _env = lock_env();
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

    /// The cluster deploy selects the node-RPC deployment and its JSON-RPC bind
    /// by env alone (`zaino-env` becomes `--set zaino.extraEnv.*`). This pins
    /// the exact keys the deploy recipe sets: `ZAINO_DEPLOYMENT` for the
    /// top-level `deployment` field (kebab-case value), and
    /// `ZAINO_SERVE__JSONRPC_LISTEN_ADDRESS` for the nested
    /// `serve.jsonrpc_listen_address` (the `__` separator crosses the one
    /// nesting level). Verified end to end through `load_config`, not just a
    /// TOML parse, so the env layering is what is tested.
    #[test]
    fn env_selects_the_node_rpc_deployment_and_jsonrpc_bind() {
        let _env = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        // A light-wallet default on disk; env must flip it to node-RPC and bind
        // the public JSON-RPC address an in-cluster deploy uses.
        let toml = r#"
network = "Mainnet"

[source]
mode = "rpc"
jsonrpc_address = "127.0.0.1:8232"

[store]
path = "/tmp/zaino-store"
"#;
        let path = write(&dir, "env-node-rpc.toml", toml);
        // nextest runs each test in its own process, so these do not leak across
        // tests; removed promptly regardless. The deploy still sets the legacy
        // `node-rpc-passthrough` value, which the loader accepts as an alias for
        // `NodeRpcLocal` — so this also pins the alias through the full loader.
        std::env::set_var("ZAINO_DEPLOYMENT", "node-rpc-passthrough");
        std::env::set_var("ZAINO_SERVE__JSONRPC_LISTEN_ADDRESS", "0.0.0.0:8232");
        let config = load_config(&path);
        std::env::remove_var("ZAINO_DEPLOYMENT");
        std::env::remove_var("ZAINO_SERVE__JSONRPC_LISTEN_ADDRESS");
        let config = config.expect("load");
        assert_eq!(config.deployment, DeploymentKind::NodeRpcLocal);
        assert_eq!(
            config.serve.jsonrpc_listen_address,
            "0.0.0.0:8232".parse().expect("valid addr"),
        );
        // The gRPC address the env did not touch keeps its default.
        assert_eq!(
            config.serve.grpc_listen_address,
            ServeConfig::default().grpc_listen_address,
        );
    }

    #[test]
    fn deferred_writes_knob_parses_defaults_and_rejects_junk() {
        // Absent: the store defers on `auto`, the default.
        let base = r#"
[store]
path = "/tmp/zaino-store"
"#;
        let config: DaemonConfig = toml::from_str(base).expect("defaults parse");
        assert_eq!(config.store.deferred_writes, DeferralPolicy::Auto);

        // `off` is honoured (the lowercase serde name).
        let off = r#"
[store]
path = "/tmp/zaino-store"
deferred_writes = "off"
"#;
        let config: DaemonConfig = toml::from_str(off).expect("off parses");
        assert_eq!(config.store.deferred_writes, DeferralPolicy::Off);

        // An unknown value is rejected at parse time, never coerced to a default.
        let junk = r#"
[store]
path = "/tmp/zaino-store"
deferred_writes = "sometimes"
"#;
        assert!(toml::from_str::<DaemonConfig>(junk).is_err());
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let _env = lock_env();
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
}
