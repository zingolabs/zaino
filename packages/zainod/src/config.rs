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
        deployment: DeploymentKind::default(),
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
            ..ServeConfig::default()
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

/// Env var selecting what the mainnet Rpc fixture's indexer fetches per height:
/// `full` (whole blocks over the standard read, which any validator answers,
/// the default) or `compact` (the fork's pre-index compact block — see
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
/// `0.0.0.0:8137`, JSON-RPC on `0.0.0.0:8232`) is baked; the deployment is
/// selected at boot by [`fixture_deployment`]. NEVER for production: gated behind BOTH the
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
        deployment: DeploymentKind::default(),
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
            jsonrpc_listen_address: SocketAddr::from(([0, 0, 0, 0], 8232)),
        },
        indexer: IndexerConfig::default(),
    }
}

/// The env var that activates the mainnet **Rpc** fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
///
/// The Rpc analogue of [`MAINNET_STATE_FIXTURE_ENV`]: it boots the greenfield
/// daemon against the validator's JSON-RPC alone (no on-disk state DB), so the
/// RPC source adapter can be exercised under a real deploy. When both this and
/// [`MAINNET_STATE_FIXTURE_ENV`] are set, the Direct/state fixture wins (see the
/// boot wiring in `lib.rs`) — set only one.
#[cfg(feature = "ztest-fixture")]
pub const MAINNET_RPC_FIXTURE_ENV: &str = "ZAINO_MAINNET_RPC_FIXTURE";

/// TEST/DEPLOY-ONLY: a mainnet **Rpc** [`DaemonConfig`] built entirely from
/// env-supplied topology, for booting the greenfield daemon against the
/// validator's JSON-RPC only (no state DB) — the Rpc counterpart of
/// [`mainnet_direct_state_fixture`]. It keeps the SAME store path so an Rpc-mode
/// deploy reuses an existing index PVC (the index is source-agnostic) rather than
/// reindexing.
///
/// Topology arrives by env so one image serves any cluster:
/// - [`TEST_FIXTURE_JSONRPC_ENV`]: the validator JSON-RPC `host:port`.
/// - [`TEST_FIXTURE_STORE_ENV`]: the writable FS-store directory.
/// - [`TEST_FIXTURE_MAP_SIZE_ENV`]: the LMDB map size in GiB (store ceiling).
/// - [`TEST_FIXTURE_FETCH_ENV`]: `compact` or `full` — what the indexer fetches
///   per height. `full` lets the fixture index from a stock validator.
///
/// The serving policy (mainnet, reorg-margin finalised depth, gRPC on
/// `0.0.0.0:8137`, JSON-RPC on `0.0.0.0:8232`) is baked; the deployment is
/// selected at boot by [`fixture_deployment`]. NEVER for production: gated behind BOTH the
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
            jsonrpc_listen_address: SocketAddr::from(([0, 0, 0, 0], 8232)),
        },
        indexer: IndexerConfig {
            fetch: fixture_fetch_strategy(),
            ..IndexerConfig::default()
        },
    }
}

/// The env var a fixture reads to select its deployment.
///
/// The same key the layered loader maps onto [`DaemonConfig::deployment`], so a
/// deploy selects the deployment once whichever config path boots. Absent means
/// the default deployment.
#[cfg(feature = "ztest-fixture")]
pub const FIXTURE_DEPLOYMENT_ENV: &str = "ZAINO_DEPLOYMENT";

/// The deployment a fixture runs: [`FIXTURE_DEPLOYMENT_ENV`] read with the
/// loader's own kebab-case names, or the default when the variable is unset.
///
/// A fixture builds its whole config from env and bypasses the layered loader,
/// so without this the deployment would be fixed to the default no matter what
/// the deploy asked for.
#[cfg(feature = "ztest-fixture")]
pub fn fixture_deployment() -> Result<DeploymentKind, IndexerError> {
    deployment_from_env_value(std::env::var(FIXTURE_DEPLOYMENT_ENV))
}

/// [`fixture_deployment`]'s parse, separated from the process environment so it
/// is testable without mutating shared env state.
#[cfg(feature = "ztest-fixture")]
fn deployment_from_env_value(
    read: Result<String, std::env::VarError>,
) -> Result<DeploymentKind, IndexerError> {
    use serde::de::IntoDeserializer as _;

    let value = match read {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(DeploymentKind::default()),
        Err(source @ std::env::VarError::NotUnicode(_)) => {
            return Err(IndexerError::FixtureDeploymentEnv(source))
        }
    };
    let deserializer: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
        value.as_str().into_deserializer();
    DeploymentKind::deserialize(deserializer)
        .map_err(|source| IndexerError::FixtureDeployment { value, source })
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

    /// Serialises the config tests that read or mutate process-global env vars.
    ///
    /// `load_config` layers the `ZAINO_*` environment over the file, and several
    /// tests `set_var`/`remove_var` to exercise that layering. Env vars are
    /// process-global, so under the default thread-based test runner a mutator in
    /// one test races a loader in another (a `set_var` leaking into an unrelated
    /// `load_config`). Every such test takes this one lock for its whole body, so
    /// they run one at a time and the default `cargo test` passes without
    /// `--test-threads=1`. Poisoning (a panicking test) is recovered with
    /// `into_inner` so one failure does not cascade into spurious lock errors.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take [`ENV_LOCK`] for the current test, recovering from poisoning.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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

    /// The mainnet Rpc fixture is a mainnet, Rpc-source config with the baked
    /// serving policy and the default (unset) JSON-RPC endpoint. nextest runs each
    /// test in its own process, so the endpoint env var does not leak.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn mainnet_rpc_fixture_is_a_mainnet_rpc_config() {
        let _env = lock_env();
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
        // Serving policy is baked; store path matches the Direct fixture so an
        // Rpc-mode deploy reuses the same index PVC.
        assert_eq!(
            config.serve.grpc_listen_address,
            "0.0.0.0:8137".parse().expect("valid addr"),
        );
        assert_eq!(
            config.store.path,
            super::mainnet_direct_state_fixture().store.path
        );
    }

    /// A fixture selects its deployment from the same kebab-case names the
    /// layered loader accepts; unset means the default, and an unknown name or a
    /// non-Unicode value is a typed error rather than a silent default.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn fixture_deployment_reads_the_loaders_names() {
        use crate::error::IndexerError;

        assert_eq!(
            super::deployment_from_env_value(Err(std::env::VarError::NotPresent))
                .expect("unset selects the default"),
            DeploymentKind::default(),
        );
        assert_eq!(
            super::deployment_from_env_value(Ok("node-rpc-local".to_owned()))
                .expect("a known deployment name"),
            DeploymentKind::NodeRpcLocal,
        );
        assert_eq!(
            super::deployment_from_env_value(Ok("light-wallet-local".to_owned()))
                .expect("a known deployment name"),
            DeploymentKind::LightWalletLocal,
        );
        assert_eq!(
            super::deployment_from_env_value(Ok("light-wallet-passthrough".to_owned()))
                .expect("a known deployment name"),
            DeploymentKind::LightWalletPassthrough,
        );
        assert!(matches!(
            super::deployment_from_env_value(Ok("node-rpc".to_owned())),
            Err(IndexerError::FixtureDeployment { value, .. }) if value == "node-rpc"
        ));
        assert!(matches!(
            super::deployment_from_env_value(Err(std::env::VarError::NotUnicode(
                std::ffi::OsString::from("x")
            ))),
            Err(IndexerError::FixtureDeploymentEnv(_))
        ));
    }

    /// The live cluster deploy sets `ZAINO_DEPLOYMENT=node-rpc-passthrough`; the
    /// renamed `NodeRpcLocal` keeps that value working through the `serde` alias,
    /// so the rename does not require a coordinated deploy change. Pinned on the
    /// fixture parse, which shares the loader's names.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn node_rpc_passthrough_alias_parses_to_node_rpc_local() {
        assert_eq!(
            super::deployment_from_env_value(Ok("node-rpc-passthrough".to_owned()))
                .expect("the legacy alias is accepted"),
            DeploymentKind::NodeRpcLocal,
        );
    }

    /// Both mainnet fixtures bake the JSON-RPC bind beside the gRPC one, so a
    /// fixture boot of the node-RPC deployment is reachable in-cluster.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn mainnet_fixtures_bind_jsonrpc_on_all_interfaces() {
        let _env = lock_env();
        let expected = SocketAddr::from(([0, 0, 0, 0], 8232));
        assert_eq!(
            super::mainnet_direct_state_fixture()
                .serve
                .jsonrpc_listen_address,
            expected
        );
        assert_eq!(
            super::mainnet_rpc_fixture().serve.jsonrpc_listen_address,
            expected
        );
    }

    /// The endpoint env override reaches the Rpc fixture.
    #[cfg(feature = "ztest-fixture")]
    #[test]
    fn mainnet_rpc_fixture_takes_the_endpoint_env() {
        let _env = lock_env();
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
