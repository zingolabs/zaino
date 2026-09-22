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
//! Greenfield, not the legacy `zaino-state` config: it carries only what the
//! runtime serving stack consumes. Config is layered highest-priority-first:
//! environment variables (`ZAINO_` prefix, `__` nesting), then the TOML file,
//! then built-in defaults.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use zaino_indexer::FetchConcurrency;

use serde::{Deserialize, Serialize};
use tracing::info;

use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;

pub use zaino_common::Network;

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
            path: zaino_common::xdg::resolve_path_with_xdg_cache_defaults("zaino/store"),
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
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
            network: Network::Mainnet,
            metrics_endpoint: None,
            source: SourceMode::default(),
            store: StoreConfig::default(),
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

/// The env var that activates the ztest regtest Direct fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_ENV: &str = "ZAINO_TEST_REGTEST_DIRECT_FIXTURE";

/// Topology bindings for the [`direct_regtest`] profile: the values that depend
/// on *where* the daemon runs, not *what* it serves. A deployer supplies these —
/// the ztest e2e today, a `generate-config` emitter later — while the profile
/// bakes everything else (tuning, network, retention).
#[cfg(feature = "ztest-fixture")]
pub(crate) struct DirectRegtestTopology {
    /// Zebra's regtest state DB, opened as a RocksDB secondary — the Direct source.
    pub(crate) zebra_cache_dir: PathBuf,
    /// The validator's JSON-RPC `host:port`; the NFS (chain-head) dials it.
    pub(crate) jsonrpc_address: String,
    /// Where the daemon listens for the CompactTxStreamer gRPC.
    pub(crate) grpc_listen_address: SocketAddr,
    /// The finalised-store (FS) database directory.
    pub(crate) store_path: PathBuf,
}

/// The `direct-regtest` serving profile: a Direct/`ReadState` source feeding the
/// FS⊕NFS composition, serving compact blocks on regtest. Binds the supplied
/// [`DirectRegtestTopology`] and bakes the tuning a regtest chain needs — index
/// to the tip with no reorg margin, small map — so a handful of mined blocks are
/// actually served.
///
/// This is the single definition of the profile. Its only caller today is
/// [`regtest_direct_fixture`]; a `generate-config` emitter will later map
/// topology flags onto this same function (and both ungate then). One spine
/// means the fixture and the real emitter cannot drift.
#[cfg(feature = "ztest-fixture")]
pub(crate) fn direct_regtest(topology: DirectRegtestTopology) -> DaemonConfig {
    let DirectRegtestTopology {
        zebra_cache_dir,
        jsonrpc_address,
        grpc_listen_address,
        store_path,
    } = topology;
    DaemonConfig {
        network: Network::Regtest,
        metrics_endpoint: None,
        source: SourceMode::Direct {
            zebra_cache_dir,
            jsonrpc_address,
            cookie_path: None,
            user: None,
            password: None,
        },
        store: StoreConfig {
            path: store_path,
            map_size_gb: 4,
        },
        serve: ServeConfig {
            grpc_listen_address,
        },
        indexer: IndexerConfig {
            // A regtest chain is a handful of blocks; index right to the tip
            // (no reorg margin) so the mined blocks are actually served.
            finalised_depth: 0,
            ..IndexerConfig::default()
        },
    }
}

/// TEST-ONLY: the [`direct_regtest`] profile bound to ztest's container topology.
///
/// ztest 0.1.21 mounts a *legacy*-schema `zainod.toml` this greenfield config
/// cannot parse (and injects no `ZAINO_` env). Rather than couple the config to
/// that legacy schema, the e2e sets [`TEST_FIXTURE_ENV`] and zainod boots this
/// instead — ignoring the mounted `--config`. Topology that varies per run (the
/// shared zebra volume, the validator's in-cluster JSON-RPC) arrives by env; the
/// rest matches ztest's container layout (writable root `/var/lib/zaino`, gRPC on
/// `0.0.0.0:8137`, regtest).
///
/// NEVER for production: gated behind BOTH the `ztest-fixture` build feature and
/// the runtime env var, and its activation logs a loud warning.
#[cfg(feature = "ztest-fixture")]
pub fn regtest_direct_fixture() -> DaemonConfig {
    // ztest's shared zebra volume mounts at a harness-chosen path, not a fixed
    // one, so the e2e passes it via `TEST_FIXTURE_ZEBRA_ENV`; fall back to the
    // default container path when unset.
    let zebra_cache_dir = std::env::var_os(TEST_FIXTURE_ZEBRA_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/zaino/zebra-db"));
    // In the ztest cluster the regtest validator's JSON-RPC lives on the
    // validator pod, not localhost, so the e2e passes its in-cluster address via
    // `TEST_FIXTURE_JSONRPC_ENV`; a plain local run falls back to the regtest
    // default. The NFS (chain-head) anchors here.
    let jsonrpc_address =
        std::env::var(TEST_FIXTURE_JSONRPC_ENV).unwrap_or_else(|_| "127.0.0.1:18232".to_string());
    direct_regtest(DirectRegtestTopology {
        zebra_cache_dir,
        jsonrpc_address,
        grpc_listen_address: "0.0.0.0:8137".parse().expect("valid fixture addr"),
        store_path: PathBuf::from("/var/lib/zaino/db"),
    })
}

/// Env var the e2e uses to hand the fixture the shared zebra volume's mount
/// path (see [`regtest_direct_fixture`]).
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_ZEBRA_ENV: &str = "ZAINO_TEST_ZEBRA_CACHE_DIR";

/// Env var the e2e uses to hand the fixture the validator's in-cluster JSON-RPC
/// address (`host:port`), which the NFS (chain-head) dials for non-final blocks
/// (see [`regtest_direct_fixture`]).
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_JSONRPC_ENV: &str = "ZAINO_TEST_ZEBRA_JSONRPC";

/// Env var handing the mainnet state fixture the writable FS-store directory
/// (see [`mainnet_direct_state_fixture`]). Optional; defaults to a cache path.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_STORE_ENV: &str = "ZAINO_TEST_STORE_DIR";

/// Env var handing the mainnet state fixture the LMDB map size in GiB — the
/// reserved store ceiling (see [`mainnet_direct_state_fixture`]). Optional;
/// defaults to a mainnet-safe value. Tune it per deploy without a rebuild, and
/// keep it below the backing volume's capacity.
#[cfg(feature = "ztest-fixture")]
pub const TEST_FIXTURE_MAP_SIZE_ENV: &str = "ZAINO_TEST_MAP_SIZE_GB";

/// Default LMDB map size (GiB) for the mainnet state fixture. A full mainnet
/// index far exceeds a regtest chain's, so this is generous headroom over what
/// the index actually occupies; the deploy caps it under the volume size.
#[cfg(feature = "ztest-fixture")]
const MAINNET_FIXTURE_MAP_SIZE_GB: usize = 64;

/// The env var that activates the mainnet Direct/state fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
///
/// The deploy-time analogue of [`TEST_FIXTURE_ENV`]: it lets the greenfield
/// daemon boot a Direct/state config against a cluster's shared read-only zebra
/// state cache when the surrounding deploy still mounts a legacy-schema
/// `zainod.toml` this loader cannot parse.
#[cfg(feature = "ztest-fixture")]
pub const MAINNET_STATE_FIXTURE_ENV: &str = "ZAINO_MAINNET_DIRECT_STATE_FIXTURE";

/// TEST/DEPLOY-ONLY: a mainnet Direct/state [`DaemonConfig`] built entirely from
/// env-supplied topology, for booting the greenfield daemon in a cluster's
/// state-mode deploy — a shared read-only zebra state cache (the Direct source)
/// plus the validator's JSON-RPC (tip / mempool / passthrough) — whose chart
/// still mounts a legacy-schema config this loader rejects.
///
/// Topology arrives by env so one image serves any cluster:
/// - [`TEST_FIXTURE_ZEBRA_ENV`]: the RO-mounted zebra cache root (Direct source).
/// - [`TEST_FIXTURE_JSONRPC_ENV`]: the validator JSON-RPC `host:port`.
/// - [`TEST_FIXTURE_STORE_ENV`]: the writable FS-store directory.
/// - [`TEST_FIXTURE_MAP_SIZE_ENV`]: the LMDB map size in GiB (store ceiling).
///
/// The serving policy (mainnet, reorg-margin finalised depth, gRPC on
/// `0.0.0.0:8137`) is baked. NEVER for production: gated behind BOTH the
/// `ztest-fixture` build feature and the runtime env var, with a loud warning on
/// activation.
#[cfg(feature = "ztest-fixture")]
pub fn mainnet_direct_state_fixture() -> DaemonConfig {
    let zebra_cache_dir = std::env::var_os(TEST_FIXTURE_ZEBRA_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/cache/zebrad-cache"));
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
        network: Network::Mainnet,
        metrics_endpoint: None,
        source: SourceMode::Direct {
            zebra_cache_dir,
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
        indexer: IndexerConfig::default(),
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
    fn direct_source_parses() {
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
        assert_eq!(config.indexer.finalised_depth, MAX_BLOCK_REORG_HEIGHT);
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
}
