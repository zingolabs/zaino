//! zainod daemon config: the one operator surface
//!
//! - Runtime crates ([`zaino_sync`], [`zaino_grpc`], source adapters) take typed params, no config
//!   (→ [`crate::indexer::start_indexer`] translates at boot)
//! - Layered highest-priority-first: env (`ZAINO_CONFIG_` prefix, `__` nesting) → TOML → defaults

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use zaino_grpc::GrpcLimits;

use serde::{Deserialize, Serialize};
use tracing::info;

use zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS;
use zaino_persistence::{manifest_fits, Fanout, IndexKind, LsmConfig};
use zaino_primitives::protocol::MAX_BLOCK_REORG_HEIGHT;
use zaino_source::ResponseSizeCap;
use zcash_protocol::consensus::NetworkType;

use crate::error::IndexerError;

/// TOML spelling of [`NetworkType`] (no serde impls upstream; `mainnet`, not `Main`)
#[derive(Deserialize, Serialize)]
#[serde(remote = "NetworkType", rename_all = "lowercase")]
enum NetworkDef {
    #[serde(rename = "mainnet")]
    Main,
    #[serde(rename = "testnet")]
    Test,
    Regtest,
}

/// The same spelling for logs, `/status` and messages (tested equal to the TOML key)
pub(crate) fn network_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "mainnet",
        NetworkType::Test => "testnet",
        NetworkType::Regtest => "regtest",
    }
}

pub(crate) const GENERATED_CONFIG_HEADER: &str = r#"# Zaino daemon configuration
#
# Generated with `zainod generate-config`.
#
# Layered highest-priority-first: ZAINO_CONFIG_ env vars, then this file, then defaults.
# For documentation see https://github.com/zingolabs/zaino
"#;

/// Zebra JSON-RPC endpoint Zaino trusts: headers for the verified chain, mempool admission (with
/// fees), mined-transaction lookups; no vote (`priority` = who is asked first)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct TrustedValidatorConfig {
    pub(crate) jsonrpc_address: String,
    pub(crate) cookie_path: Option<PathBuf>,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) connect_timeout_secs: NonZeroU64,
    /// Silence, not total duration (a multi-MB block over a slow link takes as long as it takes)
    pub(crate) read_timeout_secs: NonZeroU64,
    /// >= 5: poll + one per lane + one shared (zebrad admits 100 across every instance)
    pub(crate) max_connections: NonZeroU32,
    /// Unset = unlimited (set for a remote/shared validator)
    pub(crate) max_mib_per_sec: Option<NonZeroU32>,
    /// Largest JSON-RPC reply read: 4 (one full block) ..= 32 (batches split to fit)
    pub(crate) max_response_mib: ResponseMib,
    /// zebrad's `indexer_listen_addr` (set = push streams wake the poller, unset = poll each second)
    pub(crate) indexer_address: Option<String>,
    /// Unset = plaintext (zakurad requires it off loopback)
    pub(crate) indexer_tls: Option<ZebraIndexerTlsConfig>,
    /// Every 0 asked before any 1 (higher tier = overflow + hedges only)
    pub(crate) priority: u8,
}

impl Default for TrustedValidatorConfig {
    fn default() -> Self {
        let limits = zaino_source::LinkLimits::default();
        Self {
            jsonrpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
            connect_timeout_secs: const { whole_secs(zaino_source::Timeouts::DEFAULT.connect) },
            read_timeout_secs: const { whole_secs(zaino_source::Timeouts::DEFAULT.read) },
            max_connections: limits.max_connections,
            max_mib_per_sec: None,
            max_response_mib: ResponseMib(limits.response_size_cap),
            indexer_address: None,
            indexer_tls: None,
            priority: 0,
        }
    }
}

/// `max_response_mib`: refused at parse outside one full block's reply ..= 32 MiB
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u32", into = "u32")]
pub(crate) struct ResponseMib(ResponseSizeCap);

impl TryFrom<u32> for ResponseMib {
    type Error = String;

    fn try_from(mib: u32) -> Result<Self, String> {
        let cap = ResponseSizeCap::new((mib as usize) << 20);
        cap.map(Self).ok_or_else(|| format!("max_response_mib = {mib}: must be 4 ..= 32"))
    }
}

impl From<ResponseMib> for u32 {
    fn from(mib: ResponseMib) -> Self {
        (mib.0.bytes() >> 20) as u32
    }
}

/// `[trusted_validators.indexer_tls]`: PEM files for zakurad's `[rpc.indexer_tls]` (its
/// `client_ca_file` signs `cert_path`), read at boot
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ZebraIndexerTlsConfig {
    pub(crate) ca_path: PathBuf,
    /// Leaf first
    pub(crate) cert_path: PathBuf,
    /// PKCS#8, PKCS#1 or SEC1
    pub(crate) key_path: PathBuf,
    /// Unset = the `indexer_address` host
    pub(crate) server_name: Option<String>,
}

impl TrustedValidatorConfig {
    /// The balancer's budget; `None` = `max_connections` below
    /// [`Limits::MIN_CONNECTIONS`](zaino_traffic::Limits) (refused by [`DaemonConfig::validate`])
    pub(crate) fn limits(&self) -> Option<zaino_traffic::Limits> {
        zaino_traffic::Limits::new(self.max_connections.get())
    }

    /// The transport's: connection pool + response bytes paced as read
    pub(crate) fn link(&self) -> zaino_source::LinkLimits {
        let mib = const { NonZeroU32::new(1 << 20).expect("2^20 is non-zero") };
        let max_bytes_per_sec = self.max_mib_per_sec.map(|per_sec| per_sec.saturating_mul(mib));
        zaino_source::LinkLimits {
            max_connections: self.max_connections,
            max_bytes_per_sec,
            response_size_cap: self.max_response_mib.0,
        }
    }

    pub(crate) fn indexer_tls(&self) -> Option<zaino_source::ZebraIndexerTls> {
        self.indexer_tls.as_ref().map(|tls| zaino_source::ZebraIndexerTls {
            ca: tls.ca_path.clone(),
            cert: tls.cert_path.clone(),
            key: tls.key_path.clone(),
            server_name: tls.server_name.clone(),
        })
    }
}

impl From<&TrustedValidatorConfig> for zaino_source::Timeouts {
    fn from(config: &TrustedValidatorConfig) -> Self {
        Self {
            connect: Duration::from_secs(config.connect_timeout_secs.get()),
            read: Duration::from_secs(config.read_timeout_secs.get()),
        }
    }
}

/// One enabled index, resolved: its directory + the shared `[sync]` queue budget (boot's input;
/// its write buffer = the index crate's `WRITE_BUFFER`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexConfig {
    pub(crate) path: PathBuf,
    pub(crate) queue_bytes: NonZeroUsize,
}

/// `[index.*]`: which indexes this daemon builds and serves, one directory each
///
/// - read through [`DaemonConfig::enabled`] (resolves `[sync]` in, and utxo_set)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "IndexesToml")]
pub(crate) struct Indexes {
    compact_block: IndexTable,
    block_hash: IndexTable,
    tree_state: IndexTable,
    transparent_address: IndexTable,
}

impl Default for Indexes {
    fn default() -> Self {
        IndexesToml::default().into()
    }
}

/// One `[index.<name>]` table (`path` created if absent)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IndexTable {
    /// Off = its methods answer `UNIMPLEMENTED`
    enabled: bool,
    path: PathBuf,
}

/// `[index.*]` as written (omitted `path` = `<cache dir>/zaino/indexes/<kind.name()>`)
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct IndexesToml {
    compact_block: IndexTableToml,
    block_hash: IndexTableToml,
    tree_state: IndexTableToml,
    transparent_address: IndexTableToml,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct IndexTableToml {
    enabled: bool,
    path: Option<PathBuf>,
}

impl Default for IndexTableToml {
    fn default() -> Self {
        Self { enabled: true, path: None }
    }
}

impl IndexTableToml {
    fn resolve(self, kind: IndexKind) -> IndexTable {
        let path = self.path.unwrap_or_else(|| crate::paths::default_index(kind));
        IndexTable { enabled: self.enabled, path }
    }
}

impl From<IndexesToml> for Indexes {
    fn from(toml: IndexesToml) -> Self {
        Self {
            compact_block: toml.compact_block.resolve(IndexKind::CompactBlock),
            block_hash: toml.block_hash.resolve(IndexKind::BlockHash),
            tree_state: toml.tree_state.resolve(IndexKind::TreeState),
            transparent_address: toml.transparent_address.resolve(IndexKind::TransparentAddress),
        }
    }
}

/// A MiB knob in bytes (saturating: past `usize` = no limit anyway)
fn mib(value: NonZeroU32) -> NonZeroUsize {
    let mib = const { NonZeroUsize::new(1 << 20).expect("2^20 is non-zero") };
    NonZeroUsize::try_from(value).map_or(NonZeroUsize::MAX, |value| value.saturating_mul(mib))
}

/// A config default in whole, non-zero seconds (anything else = a compile error)
const fn whole_secs(duration: Duration) -> NonZeroU64 {
    assert!(duration.subsec_nanos() == 0, "a config default is whole seconds");
    NonZeroU64::new(duration.as_secs()).expect("a config default is non-zero")
}

/// Wallet-facing gRPC server
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ServeConfig {
    pub(crate) grpc_listen_address: SocketAddr,
    /// Across one request's addresses, whole history (over = `RESOURCE_EXHAUSTED`, never partial)
    pub(crate) max_address_rows: NonZeroUsize,
    /// Absent = plaintext HTTP/2 (for a TLS-terminating proxy)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tls: Option<TlsConfig>,
    /// Kept across restarts (`zainod hold` writes its marker here)
    pub(crate) state_dir: PathBuf,
    /// >= load balancer's mark-down time + DNS TTL
    pub(crate) hold_goaway_delay_secs: u64,
}

/// PEM pair the gRPC listener terminates TLS with (rustls + ring)
///
/// - Re-read within a minute of a change (renewal = no restart); a failing pair keeps the old one
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsConfig {
    /// Leaf first
    pub(crate) cert_path: PathBuf,
    /// PKCS#8, PKCS#1 or SEC1
    pub(crate) key_path: PathBuf,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            grpc_listen_address: SocketAddr::from(([127, 0, 0, 1], 8137)),
            max_address_rows: DEFAULT_MAX_ADDRESS_ROWS,
            tls: None,
            state_dir: crate::paths::default_state(),
            hold_goaway_delay_secs: 0,
        }
    }
}

impl ServeConfig {
    pub(crate) fn hold_goaway_delay(&self) -> std::time::Duration {
        Duration::from_secs(self.hold_goaway_delay_secs)
    }
}

/// gRPC caps: refuse, never queue (over a cap: connection closed at accept, stream `UNAVAILABLE`
/// + retry hint; read lanes excepted: a read waits for its lane's permit)
///
/// - Omitted key = `zaino_grpc::GrpcLimits::default()`'s value
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct GrpcConfig {
    /// One fd each (must fit the open-file limit, else zainod refuses to start)
    pub(crate) max_connections: NonZeroUsize,
    /// Client = peer address, or the PROXY header's behind a `trusted_proxies` entry
    pub(crate) max_connections_per_ip: NonZeroUsize,
    /// HTTP/2 `SETTINGS_MAX_CONCURRENT_STREAMS` (every stream, health included)
    pub(crate) max_streams_per_connection: NonZeroU32,
    /// Every method but `GetMempoolStream`, across all connections
    pub(crate) max_work_streams: NonZeroUsize,
    /// `GetMempoolStream`, apart from `max_work_streams` (idle until the next block)
    pub(crate) max_subscriptions: NonZeroUsize,
    /// Block, tree state, subtree roots on the blocking pool (warm = CPU-bound: ~core count)
    pub(crate) max_point_reads: NonZeroUsize,
    /// `GetBlockRange` windows, <= 1 MiB each (= device queue depth)
    pub(crate) max_range_reads: NonZeroUsize,
    /// Transparent-address history scans (few: each walks a whole history)
    pub(crate) max_scan_reads: NonZeroUsize,
    /// Data its client leaves unread this long → connection closed
    pub(crate) unread_timeout_secs: NonZeroU64,
    /// Nothing to send this long → stream `UNAVAILABLE` (`GetMempoolStream` exempt)
    pub(crate) silent_timeout_secs: NonZeroU64,
    /// 0 = never (±10% each, then GOAWAY → client reconnects + re-resolves DNS, gRPC A9)
    pub(crate) max_connection_age_secs: u64,
    pub(crate) max_connection_age_grace_secs: u64,
    /// 0 = never (no open stream that long → GOAWAY, per-client slot freed)
    pub(crate) max_connection_idle_secs: u64,
    /// Gradient2 limit under `max_work_streams` (`false` = always `max_work_streams`)
    pub(crate) adaptive_work_streams: bool,
    /// Each connection from one opens with a PROXY v1/v2 header (none = closed, empty = no proxy)
    pub(crate) trusted_proxies: Vec<ipnet::IpNet>,
    /// After SIGTERM / SIGINT (idle `GetMempoolStream` dropped at the deadline)
    pub(crate) shutdown_grace_secs: u64,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        let limits = GrpcLimits::default();
        Self {
            max_connections: limits.max_connections,
            max_connections_per_ip: limits.max_connections_per_ip,
            max_streams_per_connection: limits.max_streams_per_connection,
            max_work_streams: limits.max_work_streams,
            max_subscriptions: limits.max_subscriptions,
            max_point_reads: limits.max_point_reads,
            max_range_reads: limits.max_range_reads,
            max_scan_reads: limits.max_scan_reads,
            unread_timeout_secs: const { whole_secs(GrpcLimits::DEFAULT.unread_timeout) },
            silent_timeout_secs: const { whole_secs(GrpcLimits::DEFAULT.silent_timeout) },
            max_connection_age_secs: limits.max_connection_age.map_or(0, |age| age.as_secs()),
            max_connection_age_grace_secs: limits.max_connection_age_grace.as_secs(),
            max_connection_idle_secs: limits.max_connection_idle.map_or(0, |idle| idle.as_secs()),
            adaptive_work_streams: limits.adaptive_work_streams,
            trusted_proxies: Vec::new(),
            shutdown_grace_secs: limits.drain_timeout.as_secs(),
        }
    }
}

impl From<&GrpcConfig> for GrpcLimits {
    fn from(config: &GrpcConfig) -> Self {
        Self {
            max_connections: config.max_connections,
            max_connections_per_ip: config.max_connections_per_ip,
            max_streams_per_connection: config.max_streams_per_connection,
            max_work_streams: config.max_work_streams,
            max_subscriptions: config.max_subscriptions,
            max_point_reads: config.max_point_reads,
            max_range_reads: config.max_range_reads,
            max_scan_reads: config.max_scan_reads,
            unread_timeout: Duration::from_secs(config.unread_timeout_secs.get()),
            silent_timeout: Duration::from_secs(config.silent_timeout_secs.get()),
            drain_timeout: Duration::from_secs(config.shutdown_grace_secs),
            max_connection_age: secs_or_never(config.max_connection_age_secs),
            max_connection_age_grace: Duration::from_secs(config.max_connection_age_grace_secs),
            max_connection_idle: secs_or_never(config.max_connection_idle_secs),
            adaptive_work_streams: config.adaptive_work_streams,
        }
    }
}

/// 0 = `None` (never)
fn secs_or_never(secs: u64) -> Option<std::time::Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// `[sync]`: the one block-fetch pipeline + the budgets every index shares
///
/// - `queue_mib` in bytes, not blocks (same size from 1 KB to 2 MB blocks)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SyncConfig {
    /// >= Zebra's reorg bound (1000) off regtest (reorg-able in memory, written once this deep)
    pub(crate) finalised_depth: NonZeroU32,
    /// Block fetches + folds in flight ahead of the next block needed
    pub(crate) concurrency: NonZeroUsize,
    /// Per index: final blocks it may trail the fetch by before the pipeline throttles
    queue_mib: NonZeroU32,
    pub(crate) lsm_store: LsmStoreConfig,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            finalised_depth: const {
                NonZeroU32::new(MAX_BLOCK_REORG_HEIGHT)
                    .expect("the consensus reorg bound is non-zero")
            },
            concurrency: const { NonZeroUsize::new(32).expect("32 is non-zero") },
            queue_mib: const { NonZeroU32::new(256).expect("256 is non-zero") },
            lsm_store: LsmStoreConfig::default(),
        }
    }
}

/// `[sync.lsm_store]`: the disk engine's map tables (one engine per process: every index shares
/// `merge_slots`)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct LsmStoreConfig {
    /// >= 2 (higher = less rewriting, larger merges)
    pub(crate) fanout: Fanout,
    /// Rest wait, smallest first
    pub(crate) merge_slots: NonZeroUsize,
    /// Every merge together, read + write (commits never paced)
    pub(crate) merge_mib_per_sec: NonZeroU32,
}

impl Default for LsmStoreConfig {
    fn default() -> Self {
        let LsmConfig { fanout, merge_slots, merge_mib_per_sec } = LsmConfig::default();
        Self { fanout, merge_slots, merge_mib_per_sec }
    }
}

impl From<&LsmStoreConfig> for LsmConfig {
    fn from(config: &LsmStoreConfig) -> Self {
        let (fanout, merge_slots) = (config.fanout, config.merge_slots);
        Self { fanout, merge_slots, merge_mib_per_sec: config.merge_mib_per_sec }
    }
}

/// `SendTransaction` → network: a trusted `checkadmission` first, then one relay per attempt
/// (through the peers with `[p2p]` on, else a random trusted validator), again when not spread in
/// time
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SubmissionConfig {
    /// Unseen beyond the nodes relayed to this long → relay again
    pub(crate) propagation_threshold_secs: NonZeroU64,
    /// With peers: then one trusted validator relays it, when still unspread
    pub(crate) max_attempts: std::num::NonZeroU8,
}

/// Zaino's own p2p peers: headers + blocks (checked, never vouched), finality on their word
/// (> 5 netgroups, no difficulty collapse), the mempool quorum (each peer's listing polled),
/// submission relays
///
/// - Serves no peer (listener on loopback, no services advertised)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct P2pConfig {
    /// Off = every submission attempt → a trusted validator
    pub(crate) enabled: bool,
    /// Mainnet peers allow one connection per IP (keep it modest)
    pub(crate) peer_target: NonZeroUsize,
    /// Empty = the network's DNS seeders (regtest has none)
    pub(crate) initial_peers: Vec<String>,
    /// zebra-network peer address cache (restart dials known peers, not only seeders)
    pub(crate) cache_dir: PathBuf,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            peer_target: zaino_peers::DEFAULT_PEER_TARGET,
            initial_peers: Vec::new(),
            cache_dir: crate::paths::default_peer_cache(),
        }
    }
}

impl P2pConfig {
    pub(crate) fn peers(&self, network: NetworkType) -> zaino_peers::PeerConfig {
        zaino_peers::PeerConfig {
            peer_target: self.peer_target,
            initial_peers: (!self.initial_peers.is_empty()).then(|| self.initial_peers.clone()),
            cache_dir: Some(self.cache_dir.clone()),
            ..zaino_peers::PeerConfig::new(network)
        }
    }
}

impl Default for SubmissionConfig {
    fn default() -> Self {
        let policy = zaino_chainview::SubmitPolicy::default();
        Self {
            propagation_threshold_secs: const {
                whole_secs(zaino_chainview::SubmitPolicy::DEFAULT.propagation_threshold)
            },
            max_attempts: policy.max_attempts,
        }
    }
}

impl From<&SubmissionConfig> for zaino_chainview::SubmitPolicy {
    fn from(config: &SubmissionConfig) -> Self {
        Self {
            propagation_threshold: Duration::from_secs(config.propagation_threshold_secs.get()),
            max_attempts: config.max_attempts,
        }
    }
}

/// Admin listener: `/metrics`, `/livez`, `/readyz`, `/statusz` (unset `listen_address` = off)
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct MetricsConfig {
    /// `0.0.0.0` = every interface (access = the host's firewall)
    pub(crate) listen_address: Option<SocketAddr>,
}

/// Empty index ← published snapshot (`snapshot` feature, `aria2c` on PATH)
///
/// - Only a missing or empty directory filled; one holding data never touched
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexSnapshotConfig {
    /// Manifest URL (each archive's URL relative to it)
    pub(crate) manifest: String,
    /// <= 16 (aria2c `--split`)
    #[serde(default = "IndexSnapshotConfig::connections_default")]
    pub(crate) connections: NonZeroU32,
}

impl IndexSnapshotConfig {
    fn connections_default() -> NonZeroU32 {
        const { NonZeroU32::new(8).expect("8 is non-zero") }
    }
}

/// `ZcashLegacyRpcClient` or `ZebraIndexerGrpcClient`, one for every validator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidatorClient {
    LegacyRpc,
    Grpc,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct DaemonConfig {
    /// Declared, never derived (regtest Zebra reports `"test"` over `getblockchaininfo`)
    #[serde(with = "NetworkDef")]
    pub(crate) network: NetworkType,
    /// `grpc` = blocks over each validator's `indexer_address` (zcashd has none: `legacy_rpc`)
    pub(crate) validator_client: ValidatorClient,
    pub(crate) metrics: MetricsConfig,
    /// >= 1 (two = first set surviving a failure)
    pub(crate) trusted_validators: Vec<TrustedValidatorConfig>,
    pub(crate) serve: ServeConfig,
    pub(crate) grpc: GrpcConfig,
    pub(crate) submission: SubmissionConfig,
    pub(crate) p2p: P2pConfig,
    pub(crate) sync: SyncConfig,
    pub(crate) index: Indexes,
    /// Absent = synced from the validator
    pub(crate) snapshot: Option<IndexSnapshotConfig>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            // Mainnet = the deployment target; testnet/regtest operators declare theirs
            network: NetworkType::Main,
            // every validator speaks it (zcashd has no indexer gRPC)
            validator_client: ValidatorClient::LegacyRpc,
            metrics: MetricsConfig::default(),
            trusted_validators: vec![TrustedValidatorConfig::default()],
            serve: ServeConfig::default(),
            grpc: GrpcConfig::default(),
            submission: SubmissionConfig::default(),
            p2p: P2pConfig::default(),
            sync: SyncConfig::default(),
            index: Indexes::default(),
            snapshot: None,
        }
    }
}

impl DaemonConfig {
    /// `kind`'s directory + the `[sync]` budgets; `None` = disabled
    ///
    /// - utxo_set = compact_block's fee index: on with it, `utxo_set` beside its `path`
    pub(crate) fn enabled(&self, kind: IndexKind) -> Option<IndexConfig> {
        let index = &self.index;
        let table = |table: &IndexTable| (table.enabled, table.path.clone());
        let (enabled, path) = match kind {
            IndexKind::CompactBlock => table(&index.compact_block),
            IndexKind::UtxoSet => {
                let compact_block = &index.compact_block;
                (compact_block.enabled, compact_block.path.with_file_name(kind.name()))
            }
            IndexKind::BlockHash => table(&index.block_hash),
            IndexKind::TreeState => table(&index.tree_state),
            IndexKind::TransparentAddress => table(&index.transparent_address),
        };
        enabled.then(|| IndexConfig { path, queue_bytes: mib(self.sync.queue_mib) })
    }

    /// `(compact_block, utxo_set)` when compact_block is enabled (its fees need the other)
    pub(crate) fn compact_block(&self) -> Result<Option<(IndexConfig, IndexConfig)>, IndexerError> {
        let Some(compact_block) = self.enabled(IndexKind::CompactBlock) else { return Ok(None) };
        if compact_block.path.file_name().is_none() {
            return Err(IndexerError::Config(format!(
                "index.compact_block.path = {}: no final directory name (utxo_set sits \
                 beside it)",
                compact_block.path.display()
            )));
        }
        let utxo_set = self.enabled(IndexKind::UtxoSet).expect("on with compact_block");
        Ok(Some((compact_block, utxo_set)))
    }

    /// `Err` = no pipeline composes from it
    pub(crate) fn validate(&self) -> Result<(), IndexerError> {
        self.validate_indexes()?;
        self.validate_validators()?;
        self.validate_p2p()?;
        self.validate_snapshot()
    }

    fn validate_indexes(&self) -> Result<(), IndexerError> {
        self.compact_block()?;
        if IndexKind::ALL.into_iter().all(|kind| self.enabled(kind).is_none()) {
            return Err(IndexerError::Config(
                "every [index.*] disabled: nothing to build or serve".to_string(),
            ));
        }
        let fanout = self.sync.lsm_store.fanout;
        for kind in IndexKind::ALL.into_iter().filter(|kind| self.enabled(*kind).is_some()) {
            if !manifest_fits(&crate::stores::schema(kind, self.network).0, fanout) {
                return Err(IndexerError::Config(format!(
                    "sync.lsm_store.fanout = {}: {}'s segment lists at that fanout outgrow its \
                     manifest (lower it)",
                    fanout.get(),
                    kind.name()
                )));
            }
        }
        let depth = self.sync.finalised_depth.get();
        if self.network != NetworkType::Regtest && depth < MAX_BLOCK_REORG_HEIGHT {
            return Err(IndexerError::Config(format!(
                "sync.finalised_depth = {depth} is below the validator's reorg bound \
                 {MAX_BLOCK_REORG_HEIGHT}: a reorg it accepts could reach committed blocks \
                 (only regtest may set less)"
            )));
        }
        Ok(())
    }

    fn validate_validators(&self) -> Result<(), IndexerError> {
        if self.trusted_validators.is_empty() {
            return Err(IndexerError::Config(
                "no [[trusted_validators]]: at least one validator is needed".to_string(),
            ));
        }
        let mut addresses = std::collections::HashSet::new();
        for validator in &self.trusted_validators {
            let address = &validator.jsonrpc_address;
            if validator.limits().is_none() {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] {address}: max_connections = {} is below {} (one for \
                     the poll, one reserved per lane + one shared)",
                    validator.max_connections,
                    zaino_traffic::Limits::MIN_CONNECTIONS
                )));
            }
            if !addresses.insert(address) {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] lists {address} twice: one validator counted as two"
                )));
            }
            if validator.indexer_tls.is_some() && validator.indexer_address.is_none() {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] {address} sets indexer_tls but no indexer_address"
                )));
            }
            if self.validator_client == ValidatorClient::Grpc && validator.indexer_address.is_none()
            {
                return Err(IndexerError::Config(format!(
                    "validator_client = \"grpc\" but [[trusted_validators]] {address} has no \
                     indexer_address (zebrad's indexer_listen_addr)"
                )));
            }
        }
        Ok(())
    }

    fn validate_p2p(&self) -> Result<(), IndexerError> {
        if !self.p2p.enabled {
            return Ok(());
        }
        let peer_target = self.p2p.peer_target.get();
        if peer_target < zaino_traffic::Peers::MIN_IN_FLIGHT as usize {
            return Err(IndexerError::Config(format!(
                "p2p.peer_target = {peer_target} is below {}: the peer set carries one request \
                 per lane at once (headers, tip blocks, bulk blocks)",
                zaino_traffic::Peers::MIN_IN_FLIGHT
            )));
        }
        if self.network == NetworkType::Regtest && self.p2p.initial_peers.is_empty() {
            return Err(IndexerError::Config(
                "p2p.enabled on regtest with no p2p.initial_peers: regtest has no DNS seeders"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn validate_snapshot(&self) -> Result<(), IndexerError> {
        let Some(snapshot) = &self.snapshot else { return Ok(()) };
        if !cfg!(feature = "snapshot") {
            return Err(IndexerError::Config(
                "[snapshot] is set, but zainod was built without the `snapshot` feature"
                    .to_string(),
            ));
        }
        if snapshot.connections.get() > 16 {
            return Err(IndexerError::Config(format!(
                "snapshot.connections = {}: aria2c allows at most 16 per server",
                snapshot.connections
            )));
        }
        Ok(())
    }

    pub(crate) fn warn_about_metrics_listener(&self) {
        let Some(endpoint) = self.metrics.listen_address else {
            return;
        };
        // - Public bind publishes chain tip, sync progress, request volumes, RSS
        // - Warn, not reject (read-only telemetry; containers bind 0.0.0.0 by norm)
        if !is_private_listen_addr(&endpoint) {
            tracing::warn!(
                %endpoint,
                "metrics.listen_address binds a non-private address; /metrics is \
                 unauthenticated and exposes operational detail. Restrict it to \
                 loopback, a private interface, or a network only the scraper reaches."
            );
        }
    }
}

fn is_private_listen_addr(addr: &SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ipv4) => ipv4.is_private() || ipv4.is_loopback(),
        std::net::IpAddr::V6(ipv6) => ipv6.is_unique_local() || ipv6.is_loopback(),
    }
}

/// Defaults as TOML, behind [`GENERATED_CONFIG_HEADER`]
fn generate_default_config() -> Result<String, IndexerError> {
    let toml = toml::to_string_pretty(&DaemonConfig::default())
        .map_err(|e| IndexerError::Config(format!("serialising default config: {e}")))?;
    Ok(format!("{GENERATED_CONFIG_HEADER}{toml}"))
}

/// `zainod generate-config`: defaults → `path` (parent directories created)
pub fn generate_config(path: &Path) -> Result<ExitCode, IndexerError> {
    let content = generate_default_config()?;
    let failed = |source| IndexerError::ConfigFile { path: path.to_owned(), source };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    std::fs::write(path, content).map_err(failed)?;
    eprintln!("Generated config file: {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Defaults → `file_path` (TOML) → env (`ZAINO_CONFIG_`, `__` nesting)
pub(crate) fn load_config(file_path: &Path) -> Result<DaemonConfig, IndexerError> {
    let settings = config::Config::builder()
        .add_source(config::File::from(file_path).format(config::FileFormat::Toml).required(true))
        .add_source(
            config::Environment::with_prefix("ZAINO_CONFIG")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()
        .map_err(|e| IndexerError::Config(format!("loading configuration: {e}")))?;

    let parsed: DaemonConfig = settings
        .try_deserialize()
        .map_err(|e| IndexerError::Config(format!("parsing configuration: {e}")))?;

    info!(path = %file_path.display(), "Config loaded");
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

    /// Per index: file over defaults, env over file, `[sync]` budgets in each, utxo_set
    /// beside compact_block (and off with it)
    #[test]
    fn layers_resolve_every_index_through_defaults_file_and_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
network = "regtest"

[[trusted_validators]]
jsonrpc_address = "127.0.0.1:18232"

[sync]
finalised_depth = 100

[index.compact_block]
path = "/srv/zaino/compact_block"

[index.tree_state]
path = "/srv/zaino/tree_state"

[index.transparent_address]
enabled = false
"#;
        let path = write(&dir, "zainod.toml", toml);
        // nextest = one process per test (no leak into another test)
        std::env::set_var("ZAINO_CONFIG_SYNC__QUEUE_MIB", "128");
        std::env::set_var("ZAINO_CONFIG_INDEX__TREE_STATE__ENABLED", "false");
        let loaded = load_config(&path);
        std::env::remove_var("ZAINO_CONFIG_SYNC__QUEUE_MIB");
        std::env::remove_var("ZAINO_CONFIG_INDEX__TREE_STATE__ENABLED");
        let config = loaded.expect("load");

        let n = |n: u32| NonZeroU32::new(n).expect("non-zero");
        let sync =
            SyncConfig { finalised_depth: n(100), queue_mib: n(128), ..SyncConfig::default() };
        assert_eq!(config.sync, sync);
        let at = |path: PathBuf| {
            Some(IndexConfig { path, queue_bytes: NonZeroUsize::new(128 << 20).expect("non-zero") })
        };
        let kinds = [
            IndexKind::CompactBlock,
            IndexKind::UtxoSet,
            IndexKind::BlockHash,
            IndexKind::TreeState,
            IndexKind::TransparentAddress,
        ];
        let expected = [
            at("/srv/zaino/compact_block".into()),
            at("/srv/zaino/utxo_set".into()),
            at(crate::paths::default_index(IndexKind::BlockHash)),
            None,
            None,
        ];
        assert_eq!(kinds.map(|kind| config.enabled(kind)), expected);
        let (compact_block, utxo_set) =
            config.compact_block().expect("a valid path").expect("enabled");
        assert_eq!([Some(compact_block), Some(utxo_set)], expected[..2]);

        let mut off = config;
        off.index.compact_block.enabled = false;
        let pair = off.compact_block().expect("nothing to check").map(|_| ());
        assert_eq!((pair, off.enabled(IndexKind::UtxoSet)), (None, None), "both off");
    }

    /// Each row = one rule over an otherwise valid default config (`None` = accepted)
    #[test]
    fn validate_refuses_every_config_no_pipeline_composes_from() {
        type Edit = fn(&mut DaemonConfig);
        let snapshot_refusal = match cfg!(feature = "snapshot") {
            true => "snapshot.connections = 17: aria2c allows at most 16",
            false => "built without the `snapshot` feature",
        };
        let cases: [(&str, Edit, Option<&str>); 18] = [
            ("defaults", |_| {}, None),
            (
                "fanout whose segment lists outgrow a manifest slot",
                |c| c.sync.lsm_store.fanout = Fanout::new(128).expect(">= 2"),
                Some("sync.lsm_store.fanout = 128: utxo_set's segment lists"),
            ),
            (
                "compact_block off, block_hash still served",
                |c| c.index.compact_block.enabled = false,
                None,
            ),
            (
                "every index off",
                |c| {
                    c.index.compact_block.enabled = false;
                    c.index.block_hash.enabled = false;
                    c.index.tree_state.enabled = false;
                    c.index.transparent_address.enabled = false;
                },
                Some("every [index.*] disabled"),
            ),
            (
                "no sibling for utxo_set",
                |c| c.index.compact_block.path = "/".into(),
                Some("index.compact_block.path = /"),
            ),
            (
                "finalised inside the reorg bound on mainnet",
                |c| c.sync.finalised_depth = NonZeroU32::new(999).expect("non-zero"),
                Some("sync.finalised_depth = 999 is below the validator's reorg bound"),
            ),
            (
                "finalised inside the reorg bound on testnet",
                |c| {
                    c.network = NetworkType::Test;
                    c.sync.finalised_depth = NonZeroU32::new(999).expect("non-zero");
                },
                Some("sync.finalised_depth = 999 is below the validator's reorg bound"),
            ),
            (
                "regtest may finalise sooner",
                |c| {
                    c.network = NetworkType::Regtest;
                    c.sync.finalised_depth = NonZeroU32::new(100).expect("non-zero");
                },
                None,
            ),
            ("no validator", |c| c.trusted_validators.clear(), Some("no [[trusted_validators]]")),
            (
                "one validator twice",
                |c| c.trusted_validators.push(TrustedValidatorConfig::default()),
                Some("lists 127.0.0.1:8232 twice"),
            ),
            (
                "fewer connections than poll + lanes + 1",
                |c| c.trusted_validators[0].max_connections = NonZeroU32::new(4).expect("4"),
                Some("max_connections = 4 is below 5"),
            ),
            (
                "indexer TLS with no indexer",
                |c| {
                    c.trusted_validators[0].indexer_tls = Some(ZebraIndexerTlsConfig {
                        ca_path: "/ca.pem".into(),
                        cert_path: "/cert.pem".into(),
                        key_path: "/key.pem".into(),
                        server_name: None,
                    })
                },
                Some("sets indexer_tls but no indexer_address"),
            ),
            (
                "grpc client, validator without an indexer",
                |c| c.validator_client = ValidatorClient::Grpc,
                Some("127.0.0.1:8232 has no indexer_address"),
            ),
            (
                "grpc client, every indexer set",
                |c| {
                    c.validator_client = ValidatorClient::Grpc;
                    c.trusted_validators[0].indexer_address = Some("127.0.0.1:8230".to_owned());
                },
                None,
            ),
            (
                "p2p with fewer peers than lanes",
                |c| {
                    c.p2p.enabled = true;
                    c.p2p.peer_target = NonZeroUsize::new(2).expect("2");
                },
                Some("p2p.peer_target = 2 is below 3"),
            ),
            (
                "regtest p2p with nobody to dial",
                |c| {
                    c.network = NetworkType::Regtest;
                    c.p2p.enabled = true;
                },
                Some("regtest has no DNS seeders"),
            ),
            (
                "snapshot past aria2c's connection cap",
                |c| {
                    c.snapshot = Some(IndexSnapshotConfig {
                        manifest: "https://snapshots.example/manifest.json".to_owned(),
                        connections: NonZeroU32::new(17).expect("17"),
                    })
                },
                Some(snapshot_refusal),
            ),
            (
                "p2p off ignores its peer target",
                |c| c.p2p.peer_target = NonZeroUsize::new(2).expect("2"),
                None,
            ),
        ];
        for (case, edit, refusal) in cases {
            let mut config = DaemonConfig::default();
            edit(&mut config);
            let refused = config.validate().err().map(|error| error.to_string());
            match (refusal, &refused) {
                (Some(expected), Some(refused)) => {
                    assert!(refused.contains(expected), "{case}: {refused}")
                }
                _ => assert_eq!(refused.as_deref(), refusal, "{case}"),
            }
        }
    }

    #[test]
    fn every_table_refuses_an_unknown_key() {
        let tls = "ca_path = \"/ca\"\ncert_path = \"/c\"\nkey_path = \"/k\"\n";
        for (table, toml) in [
            ("top level", "bogus = 1\n".to_owned()),
            ("[[trusted_validators]]", "[[trusted_validators]]\nbogus = 1\n".to_owned()),
            (
                "[trusted_validators.indexer_tls]",
                format!(
                    "[[trusted_validators]]\n[trusted_validators.indexer_tls]\n{tls}bogus = 1\n"
                ),
            ),
            ("[serve]", "[serve]\nbogus = 1\n".to_owned()),
            (
                "[serve.tls]",
                "[serve.tls]\ncert_path = \"/c\"\nkey_path = \"/k\"\nbogus = 1\n".to_owned(),
            ),
            ("[grpc]", "[grpc]\nbogus = 1\n".to_owned()),
            ("[submission]", "[submission]\nbogus = 1\n".to_owned()),
            ("[p2p]", "[p2p]\nbogus = 1\n".to_owned()),
            ("[metrics]", "[metrics]\nbogus = 1\n".to_owned()),
            ("[sync]", "[sync]\nbogus = 1\n".to_owned()),
            ("[sync.lsm_store]", "[sync.lsm_store]\nbogus = 1\n".to_owned()),
            ("[index]", "[index.bogus]\npath = \"/i\"\n".to_owned()),
            ("[index.block_hash]", "[index.block_hash]\nbogus = 1\n".to_owned()),
            ("[snapshot]", "[snapshot]\nmanifest = \"https://m\"\nbogus = 1\n".to_owned()),
        ] {
            let refused = toml::from_str::<DaemonConfig>(&toml).expect_err(table).to_string();
            assert!(refused.contains("unknown field `bogus`"), "{table}: {refused}");
        }
    }

    /// - `generate-config` = every key at its default, parsing back to the defaults (indexes
    ///   under `<cache dir>/zaino/indexes/<name>`)
    /// - Shipped example parses too
    #[test]
    fn generated_config_prints_every_key_at_its_default_and_the_example_parses() {
        let generated = generate_default_config().expect("generate");
        let body = generated.strip_prefix(GENERATED_CONFIG_HEADER).expect("header first");
        let defaults = DaemonConfig::default();
        assert_eq!(toml::from_str::<DaemonConfig>(body).expect("parses back"), defaults);
        let printed = |table: &str, keys: &[&str]| {
            let start = body.find(&format!("\n[{table}]\n")).expect(table);
            let section = body[start + 1..].split("\n\n").next().unwrap_or_default();
            let found: Vec<&str> =
                section.lines().skip(1).filter_map(|l| l.split(" = ").next()).collect();
            assert_eq!(found, keys, "[{table}]");
        };
        printed("sync", &["finalised_depth", "concurrency", "queue_mib"]);
        printed("sync.lsm_store", &["fanout", "merge_slots", "merge_mib_per_sec"]);
        for table in ["compact_block", "block_hash", "tree_state", "transparent_address"] {
            printed(&format!("index.{table}"), &["enabled", "path"]);
        }
        assert!(!body.contains("utxo_set"), "internal: no table of its own");
        for kind in [IndexKind::CompactBlock, IndexKind::UtxoSet, IndexKind::TreeState] {
            let path = defaults.enabled(kind).map(|index| index.path);
            assert_eq!(path, Some(crate::paths::default_index(kind)), "{kind:?}");
        }

        let example = include_str!("../../../docs/example_configs/zainod.toml");
        let example = toml::from_str::<DaemonConfig>(example).expect("example parses");
        assert!(example.validate().is_ok());
    }

    /// `generate-config` into a missing directory: created, file = the defaults
    #[test]
    fn generate_config_writes_the_defaults_under_a_new_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new/zainod.toml");
        assert_eq!(generate_config(&path).expect("writes"), ExitCode::SUCCESS);
        let written = std::fs::read_to_string(&path).expect("written");
        assert_eq!(written, generate_default_config().expect("generate"));
    }

    /// - Caps parse per field: naming one keeps the server's own defaults for the rest
    /// - Zero refused at parse time (else it serves nothing)
    #[test]
    fn grpc_caps_default_per_field_and_reject_a_zero() {
        assert_eq!(GrpcLimits::from(&GrpcConfig::default()), GrpcLimits::default());

        let partial: DaemonConfig = toml::from_str(
            r#"
[grpc]
max_work_streams = 64
unread_timeout_secs = 30
silent_timeout_secs = 45

[index.compact_block]
path = "/tmp/zaino-compact-block"
"#,
        )
        .expect("deserialise");
        let max_work_streams = NonZeroUsize::new(64).expect("64 is non-zero");
        let limits = GrpcLimits::from(&partial.grpc);
        let expected = GrpcLimits {
            max_work_streams,
            unread_timeout: Duration::from_secs(30),
            silent_timeout: Duration::from_secs(45),
            ..GrpcLimits::default()
        };
        assert_eq!(limits, expected, "what the server is bounded by is what was parsed");

        for zeroed in [
            "max_point_reads",
            "max_range_reads",
            "max_scan_reads",
            "unread_timeout_secs",
            "silent_timeout_secs",
        ] {
            let parsed = toml::from_str::<DaemonConfig>(&format!("[grpc]\n{zeroed} = 0\n"));
            assert!(parsed.is_err(), "{zeroed} = 0 serves nothing");
        }

        let lifetime: DaemonConfig = toml::from_str(
            "[grpc]\nmax_connection_age_secs = 0\nmax_connection_age_grace_secs = 60\n\
             max_connection_idle_secs = 120\nadaptive_work_streams = false\n",
        )
        .expect("deserialise");
        let limits = GrpcLimits::from(&lifetime.grpc);
        let secs = std::time::Duration::from_secs;
        let parsed = (
            limits.max_connection_age,
            limits.max_connection_age_grace,
            limits.max_connection_idle,
            limits.adaptive_work_streams,
        );
        assert_eq!(parsed, (None, secs(60), Some(secs(120)), false), "0 = never, the rest as set");
    }

    /// Stop grace → the gRPC server's drain; hold delay → the hold
    #[test]
    fn stop_grace_and_hold_delay_reach_the_server_and_the_hold() {
        let parse = |toml: &str| toml::from_str::<DaemonConfig>(toml).expect(toml);
        let secs = std::time::Duration::from_secs;

        let defaults = parse("");
        let set =
            parse("[serve]\nhold_goaway_delay_secs = 155\n[grpc]\nshutdown_grace_secs = 600\n");
        let read = |config: &DaemonConfig| {
            (GrpcLimits::from(&config.grpc).drain_timeout, config.serve.hold_goaway_delay())
        };
        assert_eq!(read(&defaults), (secs(10), secs(0)), "defaults");
        assert_eq!(read(&set), (secs(600), secs(155)), "set");
    }

    /// Declared chain: every spelling round-trips, default mainnet, `regtest` its own value (the
    /// validator reports `"test"`)
    #[test]
    fn the_network_key_round_trips_every_chain_and_defaults_to_mainnet() {
        assert_eq!(DaemonConfig::default().network, NetworkType::Main);

        for (spelling, network) in [
            ("mainnet", NetworkType::Main),
            ("testnet", NetworkType::Test),
            ("regtest", NetworkType::Regtest),
        ] {
            let parsed: DaemonConfig =
                toml::from_str(&format!("network = \"{spelling}\"")).expect(spelling);
            assert_eq!((parsed.network, network_name(network)), (network, spelling));

            let written = toml::to_string_pretty(&parsed).expect("serialise");
            assert!(written.contains(&format!("network = \"{spelling}\"")), "{written}");
            assert_eq!(toml::from_str::<DaemonConfig>(&written).expect("reparse").network, network);
        }

        assert!(toml::from_str::<DaemonConfig>(r#"network = "main""#).is_err());
    }

    #[test]
    fn sync_lsm_store_parses_into_the_engine_config_and_a_fanout_below_two_or_a_zero_is_refused() {
        let parsed = |table: &str| {
            toml::from_str::<DaemonConfig>(&format!(
                "network = \"mainnet\"\n[sync.lsm_store]\n{table}"
            ))
        };

        let config =
            parsed("fanout = 4\nmerge_slots = 1\nmerge_mib_per_sec = 300\n").expect("deserialise");
        let merge_mib_per_sec = NonZeroU32::new(300).expect("non-zero");
        let fanout = Fanout::new(4).expect(">= 2");
        let expected = LsmConfig { fanout, merge_slots: NonZeroUsize::MIN, merge_mib_per_sec };
        assert_eq!(LsmConfig::from(&config.sync.lsm_store), expected);

        let refused = parsed("fanout = 1\n").expect_err("fanout 1");
        assert!(refused.to_string().contains("fanout 1: a merge needs 2 segments"), "{refused}");
        assert!(parsed("merge_slots = 0\n").is_err(), "merge_slots non-zero");
        assert!(parsed("merge_mib_per_sec = 0\n").is_err(), "merge_mib_per_sec non-zero");
    }

    #[test]
    fn serve_tls_absent_is_plaintext_and_present_names_its_pem_pair() {
        let parse = |toml: &str| toml::from_str::<DaemonConfig>(toml).expect(toml).serve.tls;
        assert_eq!(parse("[serve]\n"), None, "absent = plaintext");
        let tls = parse("[serve.tls]\ncert_path = \"/c.pem\"\nkey_path = \"/k.pem\"\n");
        let expected = TlsConfig { cert_path: "/c.pem".into(), key_path: "/k.pem".into() };
        assert_eq!(tls, Some(expected));
    }

    /// Entries parsed as written (auth + limits + indexer mutual TLS optional), each → the
    /// balancer's limits, the transport's link, the indexer client's TLS
    #[test]
    fn trusted_validators_convert_into_limits_link_and_indexer_tls() {
        let config: DaemonConfig = toml::from_str(
            r#"
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:18232"

[[trusted_validators]]
jsonrpc_address = "zebra-eu:8232"
user = "zaino"
password = "secret"
max_connections = 8
max_mib_per_sec = 4
max_response_mib = 8
indexer_address = "zebra-eu:8230"
priority = 1

[trusted_validators.indexer_tls]
ca_path = "/etc/zaino/indexer-ca.pem"
cert_path = "/etc/zaino/indexer-client.pem"
key_path = "/etc/zaino/indexer-client-key.pem"
"#,
        )
        .expect("parses");
        let n = |n: u32| NonZeroU32::new(n).expect("non-zero");
        let local = TrustedValidatorConfig {
            jsonrpc_address: "127.0.0.1:18232".to_owned(),
            ..TrustedValidatorConfig::default()
        };
        let eu = TrustedValidatorConfig {
            jsonrpc_address: "zebra-eu:8232".to_owned(),
            user: Some("zaino".to_owned()),
            password: Some("secret".to_owned()),
            max_connections: n(8),
            max_mib_per_sec: Some(n(4)),
            max_response_mib: ResponseMib::try_from(8).expect("in bounds"),
            indexer_address: Some("zebra-eu:8230".to_owned()),
            indexer_tls: Some(ZebraIndexerTlsConfig {
                ca_path: "/etc/zaino/indexer-ca.pem".into(),
                cert_path: "/etc/zaino/indexer-client.pem".into(),
                key_path: "/etc/zaino/indexer-client-key.pem".into(),
                server_name: None,
            }),
            priority: 1,
            ..TrustedValidatorConfig::default()
        };
        assert_eq!(config.trusted_validators, [local.clone(), eu.clone()]);

        assert_eq!(eu.limits(), zaino_traffic::Limits::new(8), "the balancer's");
        let link = zaino_source::LinkLimits {
            max_connections: n(8),
            max_bytes_per_sec: Some(n(4 << 20)),
            response_size_cap: ResponseSizeCap::new(8 << 20).expect("in bounds"),
        };
        assert_eq!(eu.link(), link, "the transport's");
        for mib in [3, 33] {
            let toml = format!("[[trusted_validators]]\nmax_response_mib = {mib}\n");
            let refused = toml::from_str::<DaemonConfig>(&toml).expect_err("out of bounds");
            assert!(refused.to_string().contains("must be 4 ..= 32"), "{mib}: {refused}");
        }
        let tls = zaino_source::ZebraIndexerTls {
            ca: "/etc/zaino/indexer-ca.pem".into(),
            cert: "/etc/zaino/indexer-client.pem".into(),
            key: "/etc/zaino/indexer-client-key.pem".into(),
            server_name: None,
        };
        assert_eq!(eu.indexer_tls(), Some(tls), "the indexer client's");
        assert_eq!(local.indexer_tls(), None, "absent = plaintext");
    }

    /// `[submission]`, `[p2p]` → policy, peer config (p2p off and DNS-seeded unless asked)
    #[test]
    fn submission_and_p2p_become_what_the_daemon_runs() {
        let config: DaemonConfig = toml::from_str(
            r#"
network = "testnet"

[submission]
propagation_threshold_secs = 30
max_attempts = 6

[p2p]
enabled = true
peer_target = 12
initial_peers = ["203.0.113.7:18233"]
cache_dir = "/var/cache/zaino/peers"
"#,
        )
        .expect("parses");
        assert_eq!(
            zaino_chainview::SubmitPolicy::from(&config.submission),
            zaino_chainview::SubmitPolicy {
                propagation_threshold: Duration::from_secs(30),
                max_attempts: std::num::NonZeroU8::new(6).expect("non-zero"),
            }
        );
        let peers = config.p2p.peers(config.network);
        assert_eq!(
            (peers.network, peers.peer_target.get(), peers.initial_peers, peers.cache_dir),
            (
                NetworkType::Test,
                12,
                Some(vec!["203.0.113.7:18233".to_owned()]),
                Some(PathBuf::from("/var/cache/zaino/peers")),
            )
        );

        let defaults = DaemonConfig::default();
        assert!(!defaults.p2p.enabled, "off unless asked");
        assert_eq!(defaults.p2p.peers(NetworkType::Main).initial_peers, None, "DNS seeders");
    }
}
