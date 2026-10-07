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

/// Per-index settings. The same shape for every served index.
///
/// Durability is not a knob here: an index commits on its own block boundary, never on a
/// timer. `batch_mib` is that boundary during bulk sync; at the tip each final block commits.
///
/// `path` has no default: it is the one field that must differ per index, and a default would
/// point a second index at the first one's files.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ZainoIndexConfig {
    /// Build this index, and serve the methods it backs.
    #[serde(default = "ZainoIndexConfig::enabled_default")]
    pub(crate) enabled: bool,
    /// Storage directory, created if absent.
    pub(crate) path: PathBuf,
    /// MiB of decoded blocks per commit during bulk sync. Bigger means fewer fsyncs, more memory
    /// held until the commit, and more to redo after a crash.
    ///
    /// Measured in bytes, not blocks, so a commit stays the same size from 1 KB early-chain
    /// blocks to 2 MB full ones.
    #[serde(default = "ZainoIndexConfig::batch_mib_default")]
    pub(crate) batch_mib: NonZeroU32,
    /// MiB of decoded blocks this index may fall behind the fetch before it throttles the whole
    /// pipeline.
    ///
    /// Per index so a heavy one can absorb a burst without pacing a light one. Measured in bytes,
    /// not blocks, so the slack holds steady from 1 KB early-chain blocks to 2 MB full ones.
    #[serde(default = "ZainoIndexConfig::queue_mib_default")]
    pub(crate) queue_mib: NonZeroU32,
}

impl ZainoIndexConfig {
    fn enabled_default() -> bool {
        true
    }

    fn batch_mib_default() -> NonZeroU32 {
        NonZeroU32::new(64).expect("64 is non-zero")
    }

    fn queue_mib_default() -> NonZeroU32 {
        NonZeroU32::new(256).expect("256 is non-zero")
    }

    /// `batch_mib` in bytes (the index's bulk commit unit)
    pub(crate) fn batch_bytes(&self) -> NonZeroUsize {
        mib(self.batch_mib)
    }

    /// `queue_mib` in bytes (the sink's budget unit)
    pub(crate) fn queue_bytes(&self) -> NonZeroUsize {
        mib(self.queue_mib)
    }

    /// Defaults for `kind`'s index (its directory = the only thing that differs between them)
    fn for_index(kind: IndexKind) -> Self {
        Self {
            enabled: Self::enabled_default(),
            path: crate::paths::default_index(kind),
            batch_mib: Self::batch_mib_default(),
            queue_mib: Self::queue_mib_default(),
        }
    }

    fn compact_block() -> Self {
        Self::for_index(IndexKind::CompactBlock)
    }

    fn block_hash() -> Self {
        Self::for_index(IndexKind::BlockHash)
    }

    fn tree_state() -> Self {
        Self::for_index(IndexKind::TreeState)
    }

    fn transparent_address() -> Self {
        Self::for_index(IndexKind::TransparentAddress)
    }

    fn value_balance() -> Self {
        Self::for_index(IndexKind::ValueBalance)
    }
}

/// A MiB knob in bytes (saturating: past `usize` means "no limit" anyway)
fn mib(value: NonZeroU32) -> NonZeroUsize {
    let bytes = usize::try_from(u64::from(value.get()) << 20).unwrap_or(usize::MAX);
    NonZeroUsize::new(bytes).expect("non-zero MiB → non-zero bytes")
}

/// The indexes this daemon builds and serves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct IndexConfig {
    /// Compact blocks: the light-wallet sync path.
    #[serde(default = "ZainoIndexConfig::compact_block")]
    pub(crate) compact_block: ZainoIndexConfig,
    /// Block hash → height: `GetBlock` and `GetTreeState` by hash (off = those `Unimplemented`).
    #[serde(default = "ZainoIndexConfig::block_hash")]
    pub(crate) block_hash: ZainoIndexConfig,
    /// Commitment trees: `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots`.
    #[serde(default = "ZainoIndexConfig::tree_state")]
    pub(crate) tree_state: ZainoIndexConfig,
    /// Transparent receives and spends: `GetAddressUtxos*`, `GetTaddressBalance*`.
    #[serde(default = "ZainoIndexConfig::transparent_address")]
    pub(crate) transparent_address: ZainoIndexConfig,
    /// Every transparent output's value: each transaction's fee in `CompactTx.fee` (the
    /// compact-block index reads it, so it cannot be disabled).
    #[serde(default = "ZainoIndexConfig::value_balance")]
    pub(crate) value_balance: ZainoIndexConfig,
    /// Every final block header, verified from genesis (proof of work, difficulty, time,
    /// linkage): the best chain everything else follows. Always on.
    pub(crate) header_chain: HeaderChainConfig,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            compact_block: ZainoIndexConfig::compact_block(),
            block_hash: ZainoIndexConfig::block_hash(),
            tree_state: ZainoIndexConfig::tree_state(),
            transparent_address: ZainoIndexConfig::transparent_address(),
            value_balance: ZainoIndexConfig::value_balance(),
            header_chain: HeaderChainConfig::default(),
        }
    }
}

/// Where the verified header chain lives (one 88-byte record per final height, ~280 MB mainnet).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct HeaderChainConfig {
    /// Directory of the header store.
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

/// The one block-fetch pipeline every index shares (sync = I/O bound on validator RPC).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct FetchConfig {
    /// Blocks below the tip kept reorg-able: in memory and served, written only once buried
    /// this deep. Default and minimum on mainnet and testnet: Zebra's reorg bound (1000); only
    /// regtest may set less.
    pub(crate) finalised_depth: NonZeroU32,
    /// Block fetches (and decodes) kept in flight during bulk sync.
    pub(crate) concurrency: NonZeroUsize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            finalised_depth: NonZeroU32::new(MAX_BLOCK_REORG_HEIGHT)
                .expect("the consensus reorg bound is non-zero"),
            concurrency: NonZeroUsize::new(32).expect("32 is non-zero"),
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
    /// The shared block-fetch pipeline.
    pub(crate) fetch: FetchConfig,
    /// How submitted transactions are pushed into the network.
    pub(crate) submission: SubmissionConfig,
    /// Zaino's own peers (`[p2p]`), off by default.
    pub(crate) p2p: P2pConfig,
    /// The indexes this daemon builds and serves.
    pub(crate) index: IndexConfig,
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
            fetch: FetchConfig::default(),
            submission: SubmissionConfig::default(),
            p2p: P2pConfig::default(),
            index: IndexConfig::default(),
            snapshot: None,
        }
    }
}

impl DaemonConfig {
    /// Reject a config the pipeline cannot be composed from.
    ///
    /// Refused: `index.compact_block.enabled = false` (its finalised height trims the chain head
    /// and answers `GetLightdInfo.blockHeight`) and `index.value_balance.enabled = false` (the
    /// compact-block index waits on its fees)
    pub(crate) fn validate(&self) -> Result<(), IndexerError> {
        if !self.index.compact_block.enabled {
            return Err(IndexerError::ConfigError(
                "index.compact_block.enabled = false: the compact-block index is load-bearing \
                 (GetLightdInfo height) and cannot be disabled"
                    .to_string(),
            ));
        }
        if !self.index.value_balance.enabled {
            return Err(IndexerError::ConfigError(
                "index.value_balance.enabled = false: the compact-block index reads every \
                 transaction's fee from it, so it cannot be disabled"
                    .to_string(),
            ));
        }
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
        let depth = self.fetch.finalised_depth.get();
        if self.network != NetworkType::Regtest && depth < MAX_BLOCK_REORG_HEIGHT {
            return Err(IndexerError::ConfigError(format!(
                "fetch.finalised_depth = {depth} is below the validator's reorg bound \
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

    /// Defaults survive a TOML round trip, and every index gets its own directory — two
    /// indexes sharing one would interleave unrelated records in the same files.
    #[test]
    fn defaults_round_trip_through_toml_with_one_directory_per_index() {
        let original = DaemonConfig::default();
        let toml = toml::to_string_pretty(&original).expect("serialise");
        let parsed: DaemonConfig = toml::from_str(&toml).expect("deserialise");
        assert_eq!(original, parsed);

        let index = &original.index;
        let paths = [
            &index.compact_block.path,
            &index.block_hash.path,
            &index.tree_state.path,
            &index.transparent_address.path,
            &index.value_balance.path,
        ];
        let distinct: std::collections::BTreeSet<_> = paths.iter().collect();
        assert_eq!(distinct.len(), 5, "{paths:?}");
        assert!(index.compact_block.path.ends_with("indexes/compact_block"));
        assert!(index.block_hash.path.ends_with("indexes/block_hash"));
        assert!(index.tree_state.path.ends_with("indexes/tree_state"));
        assert!(index.transparent_address.path.ends_with("indexes/transparent_address"));
        assert!(index.value_balance.path.ends_with("indexes/value_balance"));
        assert!(index.header_chain.path.ends_with("indexes/header_chain"));

        // Every index shares the rest of the shape, and a sub-table an operator omitted keeps
        // its own default path rather than inheriting the first index's.
        let partial: DaemonConfig = toml::from_str(
            r#"
[index.compact_block]
path = "/tmp/zaino-only-this-one"
"#,
        )
        .expect("deserialise");
        let mut only_path = ZainoIndexConfig::compact_block();
        only_path.path = PathBuf::from("/tmp/zaino-only-this-one");
        assert_eq!(partial.index.compact_block, only_path);
        assert_eq!(partial.index.block_hash, index.block_hash);
        assert_eq!(partial.index.tree_state, index.tree_state);
        assert_eq!(partial.index.transparent_address, index.transparent_address);
        assert_eq!(partial.index.value_balance, index.value_balance);
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

    /// The compact-block index cannot be turned off (the chain head and `GetLightdInfo` read its
    /// finalised height), nor the value-balance index it reads fees from; the served-only
    /// indexes are free to be, together or apart
    #[test]
    fn only_the_load_bearing_indexes_refuse_to_be_disabled() {
        let disabled =
            |toml: &str| toml::from_str::<DaemonConfig>(toml).expect("deserialise").validate();

        for index in ["compact_block", "value_balance"] {
            let err = disabled(&format!(
                "[index.{index}]\nenabled = false\npath = \"/tmp/zaino-{index}\""
            ))
            .expect_err("load-bearing");
            assert!(err.to_string().contains(&format!("index.{index}.enabled")), "{err}");
        }

        assert!(disabled(
            r#"
[index.tree_state]
enabled = false
path = "/tmp/zaino-ts"

[index.transparent_address]
enabled = false
path = "/tmp/zaino-ta"
"#
        )
        .is_ok());
        assert!(DaemonConfig::default().validate().is_ok());
    }

    /// Mainnet and testnet cannot finalise inside the validator's reorg bound (a reorg it
    /// accepts would reach committed blocks); regtest can, which keeps its tests fast
    #[test]
    fn a_finalised_depth_below_the_reorg_bound_is_refused_except_on_regtest() {
        let validated = |network: &str, depth: u32| {
            toml::from_str::<DaemonConfig>(&format!(
                "network = \"{network}\"\n[fetch]\nfinalised_depth = {depth}\n"
            ))
            .expect("deserialise")
            .validate()
        };

        let below = "fetch.finalised_depth = 999 is below the validator's reorg bound";
        for network in ["mainnet", "testnet"] {
            let err = validated(network, MAX_BLOCK_REORG_HEIGHT - 1).expect_err(network);
            assert!(err.to_string().contains(below), "{network}: {err}");
            assert!(validated(network, MAX_BLOCK_REORG_HEIGHT).is_ok(), "{network}");
            assert!(validated(network, MAX_BLOCK_REORG_HEIGHT + 1).is_ok(), "{network}");
        }
        assert!(validated("regtest", 100).is_ok());
    }

    #[test]
    fn generated_config_is_valid_toml_with_header() {
        let content = generate_default_config().expect("generate");
        assert!(content.starts_with(GENERATED_CONFIG_HEADER));
        let body = content.strip_prefix(GENERATED_CONFIG_HEADER).expect("header present");
        toml::from_str::<DaemonConfig>(body).expect("body parses");
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

    #[test]
    fn env_overrides_a_scalar_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:8232"

[index.compact_block]
path = "/tmp/zaino-compact-block"
"#;
        let path = write(&dir, "env.toml", toml);
        // A leaf scalar override applies over the file value. nextest runs each
        // test in its own process, so this env var does not leak across tests.
        std::env::set_var("ZAINO_CONFIG_FETCH__FINALISED_DEPTH", "42");
        let config = load_config(&path).expect("load");
        std::env::remove_var("ZAINO_CONFIG_FETCH__FINALISED_DEPTH");
        assert_eq!(config.fetch.finalised_depth.get(), 42);
        let path = config.index.compact_block.path.to_str();
        assert_eq!(path, Some("/tmp/zaino-compact-block"), "another key's override leaves it");
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
bogus_field = true

[index.compact_block]
path = "/tmp/zaino-compact-block"
"#;
        let path = write(&dir, "bogus.toml", toml);
        assert!(load_config(&path).is_err());
    }
}
