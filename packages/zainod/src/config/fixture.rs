//! TEST/DEPLOY-ONLY config fixtures, gated behind the `ztest-fixture` feature.
//!
//! These build a whole [`DaemonConfig`] from env-supplied topology, for booting
//! the greenfield daemon where the surrounding deploy still mounts a
//! legacy-schema `zainod.toml` this loader cannot parse. Every one is activated
//! by a runtime env var in addition to the build feature, and logs a loud
//! warning on activation (see the boot wiring in `lib.rs`). NEVER for production.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

use super::{
    DaemonConfig, DeploymentKind, FetchStrategy, IndexerConfig, Network, ServeConfig, SourceMode,
    StoreConfig,
};
use crate::error::IndexerError;

/// The env var that activates the ztest regtest Direct fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
pub const TEST_FIXTURE_ENV: &str = "ZAINO_TEST_REGTEST_DIRECT_FIXTURE";

/// Topology bindings for the [`direct_regtest`] profile: the values that depend
/// on *where* the daemon runs, not *what* it serves. A deployer supplies these —
/// the ztest e2e today, a `generate-config` emitter later — while the profile
/// bakes everything else (tuning, network, retention).
struct DirectRegtestTopology {
    /// Zebra's regtest state DB, opened as a RocksDB secondary — the Direct source.
    zebra_cache_dir: PathBuf,
    /// The validator's JSON-RPC `host:port`; the NFS (chain-head) dials it.
    jsonrpc_address: String,
    /// Where the daemon listens for the CompactTxStreamer gRPC.
    grpc_listen_address: SocketAddr,
    /// The finalised-store (FS) database directory.
    store_path: PathBuf,
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
fn direct_regtest(topology: DirectRegtestTopology) -> DaemonConfig {
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

/// TEST-ONLY: the `direct_regtest` profile bound to ztest's container topology.
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
pub const TEST_FIXTURE_ZEBRA_ENV: &str = "ZAINO_TEST_ZEBRA_CACHE_DIR";

/// Env var the e2e uses to hand the fixture the validator's in-cluster JSON-RPC
/// address (`host:port`), which the NFS (chain-head) dials for non-final blocks
/// (see [`regtest_direct_fixture`]).
pub const TEST_FIXTURE_JSONRPC_ENV: &str = "ZAINO_TEST_ZEBRA_JSONRPC";

/// Env var handing the mainnet state fixture the writable FS-store directory
/// (see [`mainnet_direct_state_fixture`]). Optional; defaults to a cache path.
pub const TEST_FIXTURE_STORE_ENV: &str = "ZAINO_TEST_STORE_DIR";

/// Env var handing the mainnet state fixture the LMDB map size in GiB — the
/// reserved store ceiling (see [`mainnet_direct_state_fixture`]). Optional;
/// defaults to a mainnet-safe value. Tune it per deploy without a rebuild, and
/// keep it below the backing volume's capacity.
pub const TEST_FIXTURE_MAP_SIZE_ENV: &str = "ZAINO_TEST_MAP_SIZE_GB";

/// Env var selecting what the mainnet Rpc fixture's indexer fetches per height:
/// `full` (whole blocks over the standard read, which any validator answers,
/// the default) or `compact` (the fork's pre-index compact block — see
/// [`FetchStrategy`]). Optional; an unrecognised value is reported and the
/// default kept.
pub const TEST_FIXTURE_FETCH_ENV: &str = "ZAINO_TEST_FETCH";

/// The fetch strategy the mainnet Rpc fixture's env selects, default when unset.
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
const MAINNET_FIXTURE_MAP_SIZE_GB: usize = 64;

/// The env var that activates the mainnet Direct/state fixture (only with the
/// `ztest-fixture` feature). Its presence — any value — triggers it.
///
/// The deploy-time analogue of [`TEST_FIXTURE_ENV`]: it lets the greenfield
/// daemon boot a Direct/state config against a cluster's shared read-only zebra
/// state cache when the surrounding deploy still mounts a legacy-schema
/// `zainod.toml` this loader cannot parse.
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
pub const FIXTURE_DEPLOYMENT_ENV: &str = "ZAINO_DEPLOYMENT";

/// The deployment a fixture runs: [`FIXTURE_DEPLOYMENT_ENV`] read with the
/// loader's own kebab-case names, or the default when the variable is unset.
///
/// A fixture builds its whole config from env and bypasses the layered loader,
/// so without this the deployment would be fixed to the default no matter what
/// the deploy asked for.
pub fn fixture_deployment() -> Result<DeploymentKind, IndexerError> {
    deployment_from_env_value(std::env::var(FIXTURE_DEPLOYMENT_ENV))
}

/// [`fixture_deployment`]'s parse, separated from the process environment so it
/// is testable without mutating shared env state.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::lock_env;

    /// The mainnet Rpc fixture is a mainnet, Rpc-source config with the baked
    /// serving policy and the default (unset) JSON-RPC endpoint. nextest runs each
    /// test in its own process, so the endpoint env var does not leak.
    #[test]
    fn mainnet_rpc_fixture_is_a_mainnet_rpc_config() {
        let _env = lock_env();
        std::env::remove_var(TEST_FIXTURE_JSONRPC_ENV);
        let config = mainnet_rpc_fixture();
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
        assert_eq!(config.store.path, mainnet_direct_state_fixture().store.path);
    }

    /// A fixture selects its deployment from the same kebab-case names the
    /// layered loader accepts; unset means the default, and an unknown name or a
    /// non-Unicode value is a typed error rather than a silent default.
    #[test]
    fn fixture_deployment_reads_the_loaders_names() {
        assert_eq!(
            deployment_from_env_value(Err(std::env::VarError::NotPresent))
                .expect("unset selects the default"),
            DeploymentKind::default(),
        );
        assert_eq!(
            deployment_from_env_value(Ok("node-rpc-local".to_owned()))
                .expect("a known deployment name"),
            DeploymentKind::NodeRpcLocal,
        );
        assert_eq!(
            deployment_from_env_value(Ok("light-wallet-local".to_owned()))
                .expect("a known deployment name"),
            DeploymentKind::LightWalletLocal,
        );
        assert_eq!(
            deployment_from_env_value(Ok("light-wallet-passthrough".to_owned()))
                .expect("a known deployment name"),
            DeploymentKind::LightWalletPassthrough,
        );
        assert!(matches!(
            deployment_from_env_value(Ok("node-rpc".to_owned())),
            Err(IndexerError::FixtureDeployment { value, .. }) if value == "node-rpc"
        ));
        assert!(matches!(
            deployment_from_env_value(Err(std::env::VarError::NotUnicode(
                std::ffi::OsString::from("x")
            ))),
            Err(IndexerError::FixtureDeploymentEnv(_))
        ));
    }

    /// The live cluster deploy sets `ZAINO_DEPLOYMENT=node-rpc-passthrough`; the
    /// renamed `NodeRpcLocal` keeps that value working through the `serde` alias,
    /// so the rename does not require a coordinated deploy change. Pinned on the
    /// fixture parse, which shares the loader's names.
    #[test]
    fn node_rpc_passthrough_alias_parses_to_node_rpc_local() {
        assert_eq!(
            deployment_from_env_value(Ok("node-rpc-passthrough".to_owned()))
                .expect("the legacy alias is accepted"),
            DeploymentKind::NodeRpcLocal,
        );
    }

    /// Both mainnet fixtures bake the JSON-RPC bind beside the gRPC one, so a
    /// fixture boot of the node-RPC deployment is reachable in-cluster.
    #[test]
    fn mainnet_fixtures_bind_jsonrpc_on_all_interfaces() {
        let _env = lock_env();
        let expected = SocketAddr::from(([0, 0, 0, 0], 8232));
        assert_eq!(
            mainnet_direct_state_fixture().serve.jsonrpc_listen_address,
            expected
        );
        assert_eq!(mainnet_rpc_fixture().serve.jsonrpc_listen_address, expected);
    }

    /// The endpoint env override reaches the Rpc fixture.
    #[test]
    fn mainnet_rpc_fixture_takes_the_endpoint_env() {
        let _env = lock_env();
        std::env::set_var(TEST_FIXTURE_JSONRPC_ENV, "zebra.example.svc:8232");
        let config = mainnet_rpc_fixture();
        std::env::remove_var(TEST_FIXTURE_JSONRPC_ENV);
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
