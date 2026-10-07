//! The zainod daemon configuration.
//!
//! This is the **daemon-mode** config surface. The runtime stack crates
//! ([`zaino_sync`], [`zaino_grpc`],
//! the source adapters) are deliberately config-agnostic — they take typed
//! params. This module is where operator config comes through, and
//! [`crate::indexer::spawn_indexer`] translates it into those typed params at
//! boot. The wallet API will get its own, separate config; keeping this one
//! self-contained keeps that boundary clean.
//!
//! It carries only what the runtime serving stack consumes. Config is layered
//! highest-priority-first:
//! environment variables (`ZAINO_CONFIG_` prefix, `__` nesting), then the TOML file,
//! then built-in defaults.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use zaino_grpc::GrpcLimits;

use serde::{Deserialize, Serialize};
use tracing::info;

use zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS;
use zaino_persistence::IndexKind;
use zaino_primitives::protocol::MAX_BLOCK_REORG_HEIGHT;
use zcash_protocol::consensus::NetworkType;

use crate::error::IndexerError;

/// TOML spelling of [`NetworkType`], which carries no serde impls of its own.
///
/// Operator-facing names, not the upstream variant names (`mainnet` over `Main`).
#[derive(Deserialize, Serialize)]
#[serde(remote = "NetworkType", rename_all = "lowercase")]
enum NetworkDef {
    #[serde(rename = "mainnet")]
    Main,
    #[serde(rename = "testnet")]
    Test,
    Regtest,
}

/// Header prepended to a generated configuration file.
pub(crate) const GENERATED_CONFIG_HEADER: &str = r#"# Zaino daemon configuration
#
# Generated with `zainod generate-config`.
#
# Layered highest-priority-first: ZAINO_CONFIG_ env vars, then this file, then defaults.
# For documentation see https://github.com/zingolabs/zaino
"#;

/// A Zebra JSON-RPC endpoint Zaino trusts: it votes on the tip, admits mempool transactions
/// (with their fees) and answers mined-transaction lookups. Every entry is equal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct TrustedValidatorConfig {
    /// The validator's JSON-RPC listen address (`host:port`).
    pub(crate) jsonrpc_address: String,
    /// Path to the validator's auth cookie, if it uses cookie auth.
    pub(crate) cookie_path: Option<PathBuf>,
    /// JSON-RPC basic-auth user, if configured.
    pub(crate) user: Option<String>,
    /// JSON-RPC basic-auth password, if configured.
    pub(crate) password: Option<String>,
    /// Seconds to open a connection to the validator.
    pub(crate) connect_timeout_secs: NonZeroU64,
    /// Seconds without a byte from the validator before a request fails. Silence, never total
    /// duration: a multi-MB block over a slow link takes as long as it takes.
    pub(crate) read_timeout_secs: NonZeroU64,
    /// Requests in flight to this validator, at least 4: 2 for polling and broadcast, a quarter
    /// for wallet lookups, the rest for block sync. Each holds one connection; a zebrad admits
    /// 100 in total, so keep `zainod instances × this` below that for a shared validator.
    pub(crate) max_connections: NonZeroU32,
    /// Requests per second to this validator. Unset = unlimited.
    pub(crate) max_requests_per_sec: Option<NonZeroU32>,
    /// MiB per second read from this validator, paced as it is read. Unset = unlimited; set it
    /// for a remote or shared validator.
    pub(crate) max_mib_per_sec: Option<NonZeroU32>,
    /// zebrad's indexer gRPC (`indexer_listen_addr`, `host:port`): its tip and mempool push
    /// streams wake the poller at once, and polling slows to a 15 s reconcile while they are up.
    /// Unset = poll every second.
    pub(crate) indexer_address: Option<String>,
}

impl Default for TrustedValidatorConfig {
    fn default() -> Self {
        let timeouts = zaino_source::Timeouts::default();
        let limits = zaino_source::LinkLimits::default();
        Self {
            jsonrpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
            connect_timeout_secs: NonZeroU64::new(timeouts.connect.as_secs())
                .expect("the default connect timeout is whole, non-zero seconds"),
            read_timeout_secs: NonZeroU64::new(timeouts.read.as_secs())
                .expect("the default read timeout is whole, non-zero seconds"),
            max_connections: limits.max_connections(),
            max_requests_per_sec: limits.max_requests_per_sec,
            max_mib_per_sec: None,
            indexer_address: None,
        }
    }
}

impl TrustedValidatorConfig {
    /// `None` = `max_connections` below [`LinkLimits::MIN_CONNECTIONS`](zaino_source::LinkLimits)
    /// (refused by [`DaemonConfig::validate`])
    pub(crate) fn limits(&self) -> Option<zaino_source::LinkLimits> {
        let mib = NonZeroU32::new(1 << 20).expect("2^20 is non-zero");
        let bytes = self.max_mib_per_sec.map(|per_sec| per_sec.saturating_mul(mib));
        zaino_source::LinkLimits::new(self.max_connections, self.max_requests_per_sec, bytes)
    }
}

impl From<&TrustedValidatorConfig> for zaino_source::Timeouts {
    fn from(config: &TrustedValidatorConfig) -> Self {
        Self {
            connect: std::time::Duration::from_secs(config.connect_timeout_secs.get()),
            read: std::time::Duration::from_secs(config.read_timeout_secs.get()),
        }
    }
}

/// One enabled index, resolved: its directory + the shared `[sync]` budgets (boot's input)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexConfig {
    pub(crate) path: PathBuf,
    pub(crate) batch_bytes: NonZeroUsize,
    pub(crate) queue_bytes: NonZeroUsize,
}

/// `[index.*]`: which indexes this daemon builds and serves, one directory each
///
/// - read through [`DaemonConfig::enabled`] (resolves `[sync]` in, and value_balance)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "IndexesToml")]
pub(crate) struct Indexes {
    compact_block: IndexTable,
    block_hash: IndexTable,
    tree_state: IndexTable,
    transparent_address: IndexTable,
    pub(crate) header_chain: HeaderChainConfig,
}

impl Default for Indexes {
    fn default() -> Self {
        IndexesToml::default().into()
    }
}

/// One `[index.<name>]` table
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IndexTable {
    /// Build this index and serve the methods it backs (off = they answer `UNIMPLEMENTED`)
    enabled: bool,
    /// Storage directory, created if absent
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
    header_chain: HeaderChainConfig,
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
            header_chain: toml.header_chain,
        }
    }
}

/// A MiB knob in bytes (saturating: past `usize` = no limit anyway)
fn mib(value: NonZeroU32) -> NonZeroUsize {
    let bytes = usize::try_from(u64::from(value.get()) << 20).unwrap_or(usize::MAX);
    NonZeroUsize::new(bytes).expect("non-zero MiB → non-zero bytes")
}

/// `[index.header_chain]`: every final header, verified from genesis (always on, ~280 MB mainnet)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct HeaderChainConfig {
    pub(crate) path: PathBuf,
}

impl Default for HeaderChainConfig {
    fn default() -> Self {
        Self { path: crate::paths::default_index(IndexKind::HeaderChain) }
    }
}

/// The wallet-facing gRPC server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ServeConfig {
    /// Address the `CompactTxStreamer` gRPC server listens on.
    pub(crate) grpc_listen_address: SocketAddr,
    /// Most transparent receives one request may walk, across all its addresses. Over it the
    /// request is `RESOURCE_EXHAUSTED`, never a short list or a partial balance. Every address
    /// method walks the whole history, whatever height range it asks about.
    pub(crate) max_address_rows: NonZeroUsize,
    /// TLS on the gRPC listener. Absent, it serves plaintext HTTP/2 for a TLS-terminating proxy
    /// in front.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tls: Option<TlsConfig>,
}

/// PEM files the gRPC listener terminates TLS with (rustls + ring). Both are re-read within a
/// minute of changing, so a certificate renewal needs no restart; a pair that fails to load
/// keeps the previous one serving.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsConfig {
    /// Certificate chain, leaf first.
    pub(crate) cert_path: PathBuf,
    /// The certificate's private key (PKCS#8, PKCS#1 or SEC1).
    pub(crate) key_path: PathBuf,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            grpc_listen_address: "127.0.0.1:8137".parse().expect("valid default addr"),
            max_address_rows: DEFAULT_MAX_ADDRESS_ROWS,
            tls: None,
        }
    }
}

/// What the gRPC server will serve at once.
///
/// Every cap here refuses rather than queues: a connection over a cap is closed at accept, a
/// stream over one is answered `UNAVAILABLE` with a retry hint. The read lanes are the one
/// exception: an index read waits for a permit of its lane.
///
/// An omitted key takes `zaino_grpc::GrpcLimits::default()`'s value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct GrpcConfig {
    /// Connections served at once; further accepts are closed. Each is a file descriptor, so
    /// zainod refuses to start when this does not fit its open-file limit.
    pub(crate) max_connections: NonZeroUsize,
    /// Connections one client may hold, so one wallet cannot take the whole cap. The client is
    /// the peer address, or behind a `trusted_proxies` entry the address its PROXY header names.
    pub(crate) max_connections_per_ip: NonZeroUsize,
    /// HTTP/2 `SETTINGS_MAX_CONCURRENT_STREAMS`, per connection.
    pub(crate) max_streams_per_connection: NonZeroU32,
    /// Work streams (every method but `GetMempoolStream`) admitted across every connection.
    pub(crate) max_streams: NonZeroUsize,
    /// `GetMempoolStream` subscriptions admitted across every connection. Separate from
    /// `max_streams`: a subscription idles until the next block, one per connected wallet.
    pub(crate) max_subscriptions: NonZeroUsize,
    /// Point reads (one block, one tree state, a subtree-root list) in flight on the blocking
    /// pool. Warm ones are CPU-bound, so about the core count.
    pub(crate) max_point_reads: NonZeroUsize,
    /// `GetBlockRange` windows (≤ 1 MiB each) in flight: the device queue depth.
    pub(crate) max_range_reads: NonZeroUsize,
    /// Transparent-address history scans in flight. Few: each walks an address's whole
    /// history (bounded per request by `serve.max_address_rows`).
    pub(crate) max_scan_reads: NonZeroUsize,
    /// Seconds a stream may hold data its client does not read before the connection is closed
    /// (a live client that never reads would otherwise keep its permits forever).
    pub(crate) stall_timeout_secs: NonZeroU64,
    /// Proxies (CIDRs) in front of this server that open every connection with a PROXY
    /// protocol header (v1 or v2) naming the real client. A connection from one of them without
    /// a header is closed. Empty = no proxy: the peer address is the client.
    pub(crate) trusted_proxies: Vec<ipnet::IpNet>,
    /// What SIGTERM / SIGINT does to the server (`[grpc.shutdown]`).
    pub(crate) shutdown: ShutdownConfig,
}

/// Graceful shutdown, for a load balancer that routes by polling `/readyz`.
///
/// Off: the listener closes on the signal, and open connections drop once the indexes have
/// flushed. On: `/readyz` fails with `draining` while gRPC keeps serving for `delay_secs`, then
/// the listener closes and open connections get `timeout_secs` to finish their streams.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ShutdownConfig {
    /// Drain before exiting. `false` ignores both durations.
    pub(crate) enabled: bool,
    /// Seconds still serving while `/readyz` reports `draining`: at least the time the load
    /// balancer takes to mark this server down, plus its DNS TTL.
    pub(crate) delay_secs: u64,
    /// Seconds open connections get to finish once the listener closes (an idle
    /// `GetMempoolStream` never does, so it is dropped at the deadline).
    pub(crate) timeout_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self { enabled: false, delay_secs: 0, timeout_secs: 10 }
    }
}

impl ShutdownConfig {
    /// Serving after the signal, before the listener closes (zero when disabled)
    pub(crate) fn delay(&self) -> std::time::Duration {
        self.when_enabled(self.delay_secs)
    }

    /// Open connections' grace once the listener closes (zero when disabled)
    pub(crate) fn timeout(&self) -> std::time::Duration {
        self.when_enabled(self.timeout_secs)
    }

    fn when_enabled(&self, secs: u64) -> std::time::Duration {
        std::time::Duration::from_secs(if self.enabled { secs } else { 0 })
    }
}

impl Default for GrpcConfig {
    fn default() -> Self {
        let limits = GrpcLimits::default();
        Self {
            max_connections: limits.max_connections,
            max_connections_per_ip: limits.max_connections_per_ip,
            max_streams_per_connection: limits.max_streams_per_connection,
            max_streams: limits.max_streams,
            max_subscriptions: limits.max_subscriptions,
            max_point_reads: limits.max_point_reads,
            max_range_reads: limits.max_range_reads,
            max_scan_reads: limits.max_scan_reads,
            stall_timeout_secs: NonZeroU64::new(limits.stall_timeout.as_secs())
                .expect("the default stall timeout is whole, non-zero seconds"),
            trusted_proxies: Vec::new(),
            shutdown: ShutdownConfig::default(),
        }
    }
}

impl From<&GrpcConfig> for GrpcLimits {
    fn from(config: &GrpcConfig) -> Self {
        Self {
            max_connections: config.max_connections,
            max_connections_per_ip: config.max_connections_per_ip,
            max_streams_per_connection: config.max_streams_per_connection,
            max_streams: config.max_streams,
            max_subscriptions: config.max_subscriptions,
            max_point_reads: config.max_point_reads,
            max_range_reads: config.max_range_reads,
            max_scan_reads: config.max_scan_reads,
            stall_timeout: std::time::Duration::from_secs(config.stall_timeout_secs.get()),
            drain_timeout: config.shutdown.timeout(),
        }
    }
}

/// `[sync]`: the one block-fetch pipeline + the budgets every index shares
///
/// - `batch_mib` / `queue_mib` in bytes, not blocks (same size from 1 KB to 2 MB blocks)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SyncConfig {
    /// Blocks below the tip kept reorg-able: in memory and served, written once buried this
    /// deep (default + minimum off regtest: Zebra's reorg bound, 1000)
    pub(crate) finalised_depth: NonZeroU32,
    /// Block fetches (and decodes) in flight during bulk sync
    pub(crate) concurrency: NonZeroUsize,
    /// MiB of decoded blocks per index commit in bulk sync (bigger = fewer fsyncs, more memory
    /// held, more to redo after a crash; at the tip each final block commits)
    batch_mib: NonZeroU32,
    /// MiB of decoded blocks one index may trail the fetch before it throttles the pipeline
    queue_mib: NonZeroU32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            finalised_depth: NonZeroU32::new(MAX_BLOCK_REORG_HEIGHT)
                .expect("the consensus reorg bound is non-zero"),
            concurrency: NonZeroUsize::new(32).expect("32 is non-zero"),
            batch_mib: NonZeroU32::new(64).expect("64 is non-zero"),
            queue_mib: NonZeroU32::new(256).expect("256 is non-zero"),
        }
    }
}

/// How `SendTransaction` pushes a transaction into the network: one random entry per attempt (a
/// peer with `[p2p]` on, else a trusted validator), resubmitted through another when it has not
/// spread in time.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SubmissionConfig {
    /// Seconds a pushed transaction gets to be seen beyond the nodes it was sent to before it is
    /// sent through another.
    pub(crate) propagation_threshold_secs: NonZeroU64,
    /// Entries one transaction is sent to at most; with peers, one trusted validator's verdict
    /// follows when none of them got it listed.
    pub(crate) max_attempts: std::num::NonZeroU8,
}

/// Zaino's own peers on the Zcash p2p network: submission entries and `peers: x/y` sightings.
/// Zaino serves no peer (its listener stays on loopback); peers never decide the tip.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct P2pConfig {
    /// Join the network. Off = every submission attempt goes to a trusted validator.
    pub(crate) enabled: bool,
    /// Outbound peers kept (mainnet peers allow one connection per IP address: keep it modest).
    pub(crate) peer_target: NonZeroUsize,
    /// Peers dialled first; empty = the network's DNS seeders (regtest has none: list them).
    pub(crate) initial_peers: Vec<String>,
    /// zebra-network's peer address cache (a restart dials known peers, not only the seeders).
    pub(crate) cache_dir: PathBuf,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            peer_target: NonZeroUsize::new(16).expect("16 is non-zero"),
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
            propagation_threshold_secs: NonZeroU64::new(policy.propagation_threshold.as_secs())
                .expect("the default threshold is whole, non-zero seconds"),
            max_attempts: policy.max_attempts,
        }
    }
}

impl From<&SubmissionConfig> for zaino_chainview::SubmitPolicy {
    fn from(config: &SubmissionConfig) -> Self {
        Self {
            propagation_threshold: std::time::Duration::from_secs(
                config.propagation_threshold_secs.get(),
            ),
            max_attempts: config.max_attempts,
        }
    }
}

/// The admin listener: `/metrics`, `/livez`, `/readyz` and `/statusz`. Disabled when
/// `listen_address` is unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct MetricsConfig {
    /// Address the admin listener binds (`0.0.0.0` = every interface; who may reach it is the
    /// host's firewall).
    pub(crate) listen_address: Option<SocketAddr>,
}

/// Bootstrap an empty index from a published snapshot (`snapshot` feature, `aria2c` on PATH).
///
/// Only indexes whose directory is missing or empty are filled; one holding data is never touched.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotConfig {
    /// URL of the snapshot manifest: `{archive, bytes, sha256, height, network}` (`archive`
    /// relative to it).
    pub(crate) manifest: String,
    /// Parallel connections the archive downloads over (aria2c `--split`, at most 16).
    #[serde(default = "SnapshotConfig::connections_default")]
    pub(crate) connections: NonZeroU32,
}

impl SnapshotConfig {
    fn connections_default() -> NonZeroU32 {
        NonZeroU32::new(8).expect("8 is non-zero")
    }
}

/// The zainod daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct DaemonConfig {
    /// Chain this daemon serves: `mainnet`, `testnet` or `regtest`.
    ///
    /// Declared, never derived: Zebra on regtest reports its chain as `"test"` over
    /// `getblockchaininfo`, so a validator-derived value mislabels every regtest deployment.
    #[serde(with = "NetworkDef")]
    pub(crate) network: NetworkType,
    /// The admin listener (`/metrics` and the probes).
    pub(crate) metrics: MetricsConfig,
    /// The validators Zaino trusts (`[[trusted_validators]]`), at least one. Their headers
    /// feed the verified header chain, served while any of them holds its tip; one listing admits
    /// a mempool transaction. Two is the first set that survives a failure (no vote).
    pub(crate) trusted_validators: Vec<TrustedValidatorConfig>,
    /// The wallet-facing gRPC server.
    pub(crate) serve: ServeConfig,
    /// What that server will serve at once.
    pub(crate) grpc: GrpcConfig,
    /// How submitted transactions are pushed into the network.
    pub(crate) submission: SubmissionConfig,
    /// Zaino's own peers (`[p2p]`), off by default.
    pub(crate) p2p: P2pConfig,
    /// The shared block-fetch pipeline and index budgets.
    pub(crate) sync: SyncConfig,
    /// The indexes this daemon builds and serves.
    pub(crate) index: Indexes,
    /// Empty indexes bootstrapped from a snapshot. Absent = they sync from the validator.
    pub(crate) snapshot: Option<SnapshotConfig>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            // Mainnet = the deployment target; testnet/regtest operators declare theirs
            network: NetworkType::Main,
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
    /// - value_balance = compact_block's fee index: on with it, `value_balance` beside its `path`
    /// - header_chain = always on
    pub(crate) fn enabled(&self, kind: IndexKind) -> Option<IndexConfig> {
        let index = &self.index;
        let table = |table: &IndexTable| (table.enabled, table.path.clone());
        let (enabled, path) = match kind {
            IndexKind::CompactBlock => table(&index.compact_block),
            IndexKind::ValueBalance => {
                let compact_block = &index.compact_block;
                (compact_block.enabled, compact_block.path.with_file_name(kind.name()))
            }
            IndexKind::BlockHash => table(&index.block_hash),
            IndexKind::TreeState => table(&index.tree_state),
            IndexKind::TransparentAddress => table(&index.transparent_address),
            IndexKind::HeaderChain => (true, index.header_chain.path.clone()),
        };
        enabled.then(|| IndexConfig {
            path,
            batch_bytes: mib(self.sync.batch_mib),
            queue_bytes: mib(self.sync.queue_mib),
        })
    }

    /// `(compact_block, value_balance)`, both required (`Routes.compact_block` not optional)
    pub(crate) fn compact_block(&self) -> Result<(IndexConfig, IndexConfig), IndexerError> {
        let path = &self.index.compact_block.path;
        if path.file_name().is_none() {
            return Err(IndexerError::ConfigError(format!(
                "index.compact_block.path = {}: no final directory name (value_balance sits \
                 beside it)",
                path.display()
            )));
        }
        let disabled = || {
            IndexerError::ConfigError(
                "index.compact_block.enabled = false: required (block methods, GetLightdInfo \
                 height)"
                    .to_string(),
            )
        };
        let compact_block = self.enabled(IndexKind::CompactBlock).ok_or_else(disabled)?;
        let value_balance = self.enabled(IndexKind::ValueBalance).ok_or_else(disabled)?;
        Ok((compact_block, value_balance))
    }

    /// Reject a config the pipeline cannot be composed from.
    pub(crate) fn validate(&self) -> Result<(), IndexerError> {
        self.compact_block()?;
        if self.trusted_validators.is_empty() {
            return Err(IndexerError::ConfigError(
                "no [[trusted_validators]]: at least one validator is needed".to_string(),
            ));
        }
        let mut addresses = std::collections::HashSet::new();
        for validator in &self.trusted_validators {
            if validator.limits().is_none() {
                return Err(IndexerError::ConfigError(format!(
                    "[[trusted_validators]] {}: max_connections = {} is below {} (2 polling, \
                     1 wallet lookups, 1 block sync)",
                    validator.jsonrpc_address,
                    validator.max_connections,
                    zaino_source::LinkLimits::MIN_CONNECTIONS
                )));
            }
            if !addresses.insert(&validator.jsonrpc_address) {
                return Err(IndexerError::ConfigError(format!(
                    "[[trusted_validators]] lists {} twice: one validator counted as two",
                    validator.jsonrpc_address
                )));
            }
        }
        if self.p2p.enabled
            && self.network == NetworkType::Regtest
            && self.p2p.initial_peers.is_empty()
        {
            return Err(IndexerError::ConfigError(
                "p2p.enabled on regtest with no p2p.initial_peers: regtest has no DNS seeders"
                    .to_string(),
            ));
        }
        let depth = self.sync.finalised_depth.get();
        if self.network != NetworkType::Regtest && depth < MAX_BLOCK_REORG_HEIGHT {
            return Err(IndexerError::ConfigError(format!(
                "sync.finalised_depth = {depth} is below the validator's reorg bound \
                 {MAX_BLOCK_REORG_HEIGHT}: a reorg it accepts could reach committed blocks \
                 (only regtest may set less)"
            )));
        }
        if let Some(snapshot) = &self.snapshot {
            if !cfg!(feature = "snapshot") {
                return Err(IndexerError::ConfigError(
                    "[snapshot] is set, but zainod was built without the `snapshot` feature"
                        .to_string(),
                ));
            }
            if snapshot.connections.get() > 16 {
                return Err(IndexerError::ConfigError(format!(
                    "snapshot.connections = {}: aria2c allows at most 16 per server",
                    snapshot.connections
                )));
            }
        }
        Ok(())
    }

    /// Logs a warning when the admin listener binds a non-private address.
    pub(crate) fn warn_about_metrics_listener(&self) {
        let Some(endpoint) = self.metrics.listen_address else {
            return;
        };
        // Public bind publishes chain tip, sync progress, request volumes & RSS.
        // Warn, not reject: read-only telemetry, and containers bind 0.0.0.0 by norm
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

/// Whether `addr` binds only loopback or a private-network interface.
fn is_private_listen_addr(addr: &SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ipv4) => ipv4.is_private() || ipv4.is_loopback(),
        std::net::IpAddr::V6(ipv6) => ipv6.is_unique_local() || ipv6.is_loopback(),
    }
}

/// Serialize the built-in defaults into a commented example config file.
pub(crate) fn generate_default_config() -> Result<String, IndexerError> {
    let toml = toml::to_string_pretty(&DaemonConfig::default())
        .map_err(|e| IndexerError::ConfigError(format!("serialising default config: {e}")))?;
    Ok(format!("{GENERATED_CONFIG_HEADER}{toml}"))
}

/// Load configuration from a TOML file with `ZAINO_CONFIG_` environment overrides.
pub(crate) fn load_config(file_path: &std::path::Path) -> Result<DaemonConfig, IndexerError> {
    load_config_with_env(file_path, "ZAINO_CONFIG")
}

/// Load configuration with a custom environment-variable prefix.
///
/// Layering: defaults → TOML file → environment (`<prefix>_`, `__` for nesting).
pub(crate) fn load_config_with_env(
    file_path: &std::path::Path,
    env_prefix: &str,
) -> Result<DaemonConfig, IndexerError> {
    let settings = config::Config::builder()
        .add_source(config::File::from(file_path).format(config::FileFormat::Toml).required(true))
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

    /// What boot reads per index: file over defaults, env over file, `[sync]` budgets in every
    /// index, value_balance beside compact_block; a stale or misspelt key fails the load
    #[test]
    fn a_config_resolves_every_index_through_defaults_file_and_env_and_refuses_stale_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
network = "regtest"

[[trusted_validators]]
jsonrpc_address = "127.0.0.1:18232"

[sync]
finalised_depth = 100
batch_mib = 32

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
        assert!(config.validate().is_ok());

        let n = |n: u32| NonZeroU32::new(n).expect("non-zero");
        let sync = SyncConfig {
            finalised_depth: n(100),
            batch_mib: n(32),
            queue_mib: n(128),
            ..SyncConfig::default()
        };
        assert_eq!(config.sync, sync);
        let at = |path: PathBuf| {
            let bytes = |mib: usize| NonZeroUsize::new(mib << 20).expect("non-zero");
            Some(IndexConfig { path, batch_bytes: bytes(32), queue_bytes: bytes(128) })
        };
        let kinds = [
            IndexKind::CompactBlock,
            IndexKind::ValueBalance,
            IndexKind::BlockHash,
            IndexKind::TreeState,
            IndexKind::TransparentAddress,
            IndexKind::HeaderChain,
        ];
        let expected = [
            at("/srv/zaino/compact_block".into()),
            at("/srv/zaino/value_balance".into()),
            at(crate::paths::default_index(IndexKind::BlockHash)),
            None,
            None,
            at(crate::paths::default_index(IndexKind::HeaderChain)),
        ];
        assert_eq!(kinds.map(|kind| config.enabled(kind)), expected);
        let (compact_block, value_balance) = config.compact_block().expect("enabled");
        assert_eq!([Some(compact_block), Some(value_balance)], expected[..2]);

        let mut off = config.clone();
        off.index.compact_block.enabled = false;
        let err = off.validate().expect_err("compact_block off").to_string();
        assert!(err.contains("index.compact_block.enabled = false"), "{err}");
        let mut root = config;
        root.index.compact_block.path = "/".into();
        let err = root.validate().expect_err("no sibling for value_balance").to_string();
        assert!(err.contains("index.compact_block.path = /"), "{err}");

        for (stale, key) in [
            ("[index.value_balance]\npath = \"/srv/zaino/vb\"\n", "value_balance"),
            ("[fetch]\nconcurrency = 8\n", "fetch"),
            ("[index.block_hash]\nqueue_mib = 256\n", "queue_mib"),
        ] {
            let path = write(&dir, &format!("{key}.toml"), &format!("{toml}\n{stale}"));
            let err = load_config(&path).expect_err(key).to_string();
            assert!(err.contains(&format!("unknown field `{key}`")), "{key}: {err}");
        }
    }

    /// `generate-config` prints every key at its default, which parses back to the defaults
    /// (each index under `<cache dir>/zaino/indexes/<name>`); the shipped example parses too
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
        printed("sync", &["finalised_depth", "concurrency", "batch_mib", "queue_mib"]);
        for table in ["compact_block", "block_hash", "tree_state", "transparent_address"] {
            printed(&format!("index.{table}"), &["enabled", "path"]);
        }
        printed("index.header_chain", &["path"]);
        assert!(!body.contains("value_balance"), "internal: no table of its own");
        for kind in [IndexKind::CompactBlock, IndexKind::ValueBalance, IndexKind::TreeState] {
            let path = defaults.enabled(kind).map(|index| index.path);
            assert_eq!(path, Some(crate::paths::default_index(kind)), "{kind:?}");
        }

        let example = include_str!("../../../docs/example_configs/zainod.toml");
        let example = toml::from_str::<DaemonConfig>(example).expect("example parses");
        assert!(example.validate().is_ok());
    }

    /// The serve caps parse field by field: an operator who names one keeps the defaults for
    /// the rest (one source: the server's own), and a zero is refused at parse time rather than
    /// serving nothing.
    #[test]
    fn grpc_caps_default_per_field_and_reject_a_zero() {
        assert_eq!(GrpcLimits::from(&GrpcConfig::default()), GrpcLimits::default());

        let partial: DaemonConfig = toml::from_str(
            r#"
[grpc]
max_streams = 64
stall_timeout_secs = 30

[index.compact_block]
path = "/tmp/zaino-compact-block"
"#,
        )
        .expect("deserialise");
        let max_streams = NonZeroUsize::new(64).expect("64 is non-zero");
        let limits = GrpcLimits::from(&partial.grpc);
        let expected = GrpcLimits {
            max_streams,
            stall_timeout: std::time::Duration::from_secs(30),
            ..GrpcLimits::default()
        };
        assert_eq!(limits, expected, "what the server is bounded by is what was parsed");

        for zeroed in ["max_point_reads", "max_range_reads", "max_scan_reads", "stall_timeout_secs"]
        {
            let parsed = toml::from_str::<DaemonConfig>(&format!("[grpc]\n{zeroed} = 0\n"));
            assert!(parsed.is_err(), "{zeroed} = 0 serves nothing");
        }
    }

    /// `[grpc.shutdown]` is opt-in: off (the default, or `enabled = false` beside durations)
    /// exits without a readiness window or a connection grace; on carries both durations to the
    /// daemon and the server, and a misspelt key is refused rather than silently ignored.
    #[test]
    fn grpc_shutdown_is_off_unless_enabled_and_then_carries_both_durations() {
        let parse = |toml: &str| toml::from_str::<DaemonConfig>(toml).expect(toml).grpc;
        let zero = std::time::Duration::ZERO;

        for off in ["", "[grpc.shutdown]\ndelay_secs = 155\ntimeout_secs = 600\n"] {
            let grpc = parse(off);
            assert_eq!((grpc.shutdown.delay(), grpc.shutdown.timeout()), (zero, zero), "{off:?}");
            assert_eq!(GrpcLimits::from(&grpc).drain_timeout, zero, "{off:?}");
        }

        let on = parse("[grpc.shutdown]\nenabled = true\ndelay_secs = 155\ntimeout_secs = 600\n");
        let secs = std::time::Duration::from_secs;
        assert_eq!((on.shutdown.delay(), on.shutdown.timeout()), (secs(155), secs(600)));
        assert_eq!(GrpcLimits::from(&on).drain_timeout, secs(600));
        let defaults = parse("[grpc.shutdown]\nenabled = true\n");
        assert_eq!((defaults.shutdown.delay(), defaults.shutdown.timeout()), (zero, secs(10)));

        let misspelt = toml::from_str::<DaemonConfig>("[grpc.shutdown]\nshutdown_delay_secs = 1\n");
        assert!(misspelt.is_err(), "unknown [grpc.shutdown] key accepted");
    }

    /// Chain identity is declared, never derived: every spelling round-trips, the default is
    /// mainnet, and `regtest` is a value of its own — the validator reports it as `"test"`.
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
            assert_eq!(parsed.network, network);

            let written = toml::to_string_pretty(&parsed).expect("serialise");
            assert!(written.contains(&format!("network = \"{spelling}\"")), "{written}");
            assert_eq!(toml::from_str::<DaemonConfig>(&written).expect("reparse").network, network);
        }

        // Upstream variant names are not the config spelling
        assert!(toml::from_str::<DaemonConfig>(r#"network = "main""#).is_err());
    }

    /// Mainnet and testnet cannot finalise inside the validator's reorg bound (a reorg it
    /// accepts would reach committed blocks); regtest can, which keeps its tests fast
    #[test]
    fn a_finalised_depth_below_the_reorg_bound_is_refused_except_on_regtest() {
        let validated = |network: &str, depth: u32| {
            toml::from_str::<DaemonConfig>(&format!(
                "network = \"{network}\"\n[sync]\nfinalised_depth = {depth}\n"
            ))
            .expect("deserialise")
            .validate()
        };

        let below = "sync.finalised_depth = 999 is below the validator's reorg bound";
        for network in ["mainnet", "testnet"] {
            let err = validated(network, MAX_BLOCK_REORG_HEIGHT - 1).expect_err(network);
            assert!(err.to_string().contains(below), "{network}: {err}");
            assert!(validated(network, MAX_BLOCK_REORG_HEIGHT).is_ok(), "{network}");
            assert!(validated(network, MAX_BLOCK_REORG_HEIGHT + 1).is_ok(), "{network}");
        }
        assert!(validated("regtest", 100).is_ok());
    }

    #[test]
    fn serve_tls_parses_and_rejects_unknown_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = "network = \"mainnet\"\n[[trusted_validators]]\n\
                    jsonrpc_address = \"127.0.0.1:8232\"\n[serve]\n";
        let plain = load_config(&write(&dir, "plain.toml", base)).expect("no tls table");
        assert_eq!(plain.serve.tls, None, "absent = plaintext");

        let tls = format!("{base}[serve.tls]\ncert_path = \"/c.pem\"\nkey_path = \"/k.pem\"\n");
        let config = load_config(&write(&dir, "tls.toml", &tls)).expect("tls table");
        let expected = TlsConfig { cert_path: "/c.pem".into(), key_path: "/k.pem".into() };
        assert_eq!(config.serve.tls, Some(expected));

        let typo = tls.replace("key_path", "keyfile");
        let err = load_config(&write(&dir, "typo.toml", &typo)).expect_err("typo");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    /// Every entry equal, auth and limits optional; a list empty, naming one validator twice (two
    /// votes) or starving a lane is refused; every removed key fails loudly rather than ignored
    #[test]
    fn trusted_validators_parse_and_removed_keys_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:18232"

[[trusted_validators]]
jsonrpc_address = "zebra-eu:8232"
user = "zaino"
password = "secret"
max_connections = 8
max_requests_per_sec = 200
max_mib_per_sec = 4
indexer_address = "zebra-eu:8230"

[index.compact_block]
path = "/tmp/zaino-compact-block"
"#;
        let config = load_config(&write(&dir, "rpc.toml", toml)).expect("load");
        let n = |n: u32| NonZeroU32::new(n).expect("non-zero");
        let entry = |address: &str| TrustedValidatorConfig {
            jsonrpc_address: address.to_owned(),
            ..TrustedValidatorConfig::default()
        };
        let eu = TrustedValidatorConfig {
            user: Some("zaino".to_owned()),
            password: Some("secret".to_owned()),
            max_connections: n(8),
            max_requests_per_sec: Some(n(200)),
            max_mib_per_sec: Some(n(4)),
            indexer_address: Some("zebra-eu:8230".to_owned()),
            ..entry("zebra-eu:8232")
        };
        assert_eq!(config.trusted_validators, [entry("127.0.0.1:18232"), eu.clone()]);
        assert!(config.validate().is_ok());
        let limits = eu.limits().expect("8 connections suffice");
        assert_eq!(
            (limits.max_connections(), limits.max_requests_per_sec, limits.max_bytes_per_sec),
            (n(8), Some(n(200)), Some(n(4 << 20))),
        );

        let refused = |config: DaemonConfig| config.validate().expect_err("refused").to_string();
        let none = DaemonConfig { trusted_validators: Vec::new(), ..config.clone() };
        assert!(refused(none).contains("no [[trusted_validators]]"));
        let twice = vec![entry("zebra-eu:8232"), entry("zebra-eu:8232")];
        let twice = DaemonConfig { trusted_validators: twice, ..config.clone() };
        assert!(refused(twice).contains("twice"));
        let starved = vec![TrustedValidatorConfig { max_connections: n(3), ..eu }];
        let starved = DaemonConfig { trusted_validators: starved, ..config };
        assert!(refused(starved).contains("max_connections = 3 is below 4"));

        for (name, removed) in [
            ("source.toml", "[source]\njsonrpc_address = \"127.0.0.1:8232\"\n"),
            ("peers.toml", "[[chainview_peers]]\njsonrpc_address = \"127.0.0.1:8232\"\n"),
            ("primary.toml", "[fetch]\nprimary_validator = \"127.0.0.1:18232\"\n"),
            ("mode.toml", "[[trusted_validators]]\nmode = \"direct\"\n"),
            ("noderpc.toml", "[serve]\njsonrpc_listen_address = \"0.0.0.0:8232\"\n"),
        ] {
            let stale = format!("{toml}\n{removed}");
            let err = load_config(&write(&dir, name, &stale)).expect_err(name);
            assert!(err.to_string().contains("unknown field"), "{name}: {err}");
        }
    }

    /// `[submission]`, `[p2p]` and `[index.header_chain]` become the policy, peer config and store
    /// the daemon runs with; p2p is off unless asked; regtest p2p with nobody to dial is refused
    #[test]
    fn submission_p2p_and_header_chain_become_what_the_daemon_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
network = "testnet"

[[trusted_validators]]
jsonrpc_address = "127.0.0.1:18232"

[submission]
propagation_threshold_secs = 30
max_attempts = 6

[p2p]
enabled = true
peer_target = 8
initial_peers = ["203.0.113.7:18233"]
cache_dir = "/var/cache/zaino/peers"

[index.header_chain]
path = "/var/lib/zaino/header-chain"
"#;
        let config = load_config(&write(&dir, "p2p.toml", toml)).expect("load");
        assert!(config.validate().is_ok());
        assert_eq!(
            zaino_chainview::SubmitPolicy::from(&config.submission),
            zaino_chainview::SubmitPolicy {
                propagation_threshold: std::time::Duration::from_secs(30),
                max_attempts: std::num::NonZeroU8::new(6).expect("non-zero"),
            }
        );
        let peers = config.p2p.peers(config.network);
        assert_eq!(
            (peers.network, peers.peer_target.get(), peers.initial_peers, peers.cache_dir),
            (
                NetworkType::Test,
                8,
                Some(vec!["203.0.113.7:18233".to_owned()]),
                Some(PathBuf::from("/var/cache/zaino/peers")),
            )
        );
        assert_eq!(config.index.header_chain.path, PathBuf::from("/var/lib/zaino/header-chain"));

        let defaults = DaemonConfig::default();
        assert!(!defaults.p2p.enabled, "off unless asked");
        assert_eq!(defaults.p2p.peers(NetworkType::Main).initial_peers, None, "DNS seeders");
        let seedless = DaemonConfig {
            network: NetworkType::Regtest,
            p2p: P2pConfig { initial_peers: Vec::new(), ..config.p2p.clone() },
            ..config
        };
        let refused = seedless.validate().expect_err("regtest has no seeders").to_string();
        assert!(refused.contains("no DNS seeders"), "{refused}");
    }
}
