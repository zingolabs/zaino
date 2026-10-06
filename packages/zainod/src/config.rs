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

/// [`NetworkDef`]'s TOML spelling, for log lines
pub(crate) fn network_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "mainnet",
        NetworkType::Test => "testnet",
        NetworkType::Regtest => "regtest",
    }
}

/// Header prepended to a generated configuration file.
pub const GENERATED_CONFIG_HEADER: &str = r#"# Zaino daemon configuration
#
# Generated with `zainod generate-config`.
#
# Layered highest-priority-first: ZAINO_CONFIG_ env vars, then this file, then defaults.
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
    /// Seconds to open a connection to the validator.
    pub connect_timeout_secs: NonZeroU64,
    /// Seconds without a byte from the validator before a request fails. Silence, never total
    /// duration: a multi-MB block over a slow link takes as long as it takes.
    pub read_timeout_secs: NonZeroU64,
}

impl Default for SourceConfig {
    fn default() -> Self {
        let timeouts = zaino_source::Timeouts::default();
        Self {
            jsonrpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
            connect_timeout_secs: NonZeroU64::new(timeouts.connect.as_secs())
                .expect("the default connect timeout is whole, non-zero seconds"),
            read_timeout_secs: NonZeroU64::new(timeouts.read.as_secs())
                .expect("the default read timeout is whole, non-zero seconds"),
        }
    }
}

impl From<&SourceConfig> for zaino_source::Timeouts {
    fn from(config: &SourceConfig) -> Self {
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
pub struct ZainoIndexConfig {
    /// Build this index, and serve the methods it backs.
    #[serde(default = "ZainoIndexConfig::enabled_default")]
    pub enabled: bool,
    /// Storage directory, created if absent.
    pub path: PathBuf,
    /// MiB of decoded blocks per commit during bulk sync. Bigger means fewer fsyncs, more memory
    /// held until the commit, and more to redo after a crash.
    ///
    /// Measured in bytes, not blocks, so a commit stays the same size from 1 KB early-chain
    /// blocks to 2 MB full ones.
    #[serde(default = "ZainoIndexConfig::batch_mib_default")]
    pub batch_mib: NonZeroU32,
    /// MiB of decoded blocks this index may fall behind the fetch before it throttles the whole
    /// pipeline.
    ///
    /// Per index so a heavy one can absorb a burst without pacing a light one. Measured in bytes,
    /// not blocks, so the slack holds steady from 1 KB early-chain blocks to 2 MB full ones.
    #[serde(default = "ZainoIndexConfig::queue_mib_default")]
    pub queue_mib: NonZeroU32,
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

    /// Defaults for the index stored under `name`, the only thing that differs between them
    fn for_index(name: &str) -> Self {
        Self {
            enabled: Self::enabled_default(),
            path: crate::paths::default_index(name),
            batch_mib: Self::batch_mib_default(),
            queue_mib: Self::queue_mib_default(),
        }
    }

    fn compact_block() -> Self {
        Self::for_index("compact-block")
    }

    fn block_hash() -> Self {
        Self::for_index("block-hash")
    }

    fn tree_state() -> Self {
        Self::for_index("tree-state")
    }

    fn transparent_address() -> Self {
        Self::for_index("transparent-address")
    }

    fn value_balance() -> Self {
        Self::for_index("value-balance")
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
pub struct IndexConfig {
    /// Compact blocks: the light-wallet sync path.
    #[serde(default = "ZainoIndexConfig::compact_block")]
    pub compact_block: ZainoIndexConfig,
    /// Block hash → height: `GetBlock` and `GetTreeState` by hash (off = those `Unimplemented`).
    #[serde(default = "ZainoIndexConfig::block_hash")]
    pub block_hash: ZainoIndexConfig,
    /// Commitment trees: `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots`.
    #[serde(default = "ZainoIndexConfig::tree_state")]
    pub tree_state: ZainoIndexConfig,
    /// Transparent receives and spends: `GetAddressUtxos*`, `GetTaddressBalance*`.
    #[serde(default = "ZainoIndexConfig::transparent_address")]
    pub transparent_address: ZainoIndexConfig,
    /// Every transparent output's value: each transaction's fee in `CompactTx.fee` (the
    /// compact-block index reads it, so it cannot be disabled).
    #[serde(default = "ZainoIndexConfig::value_balance")]
    pub value_balance: ZainoIndexConfig,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            compact_block: ZainoIndexConfig::compact_block(),
            block_hash: ZainoIndexConfig::block_hash(),
            tree_state: ZainoIndexConfig::tree_state(),
            transparent_address: ZainoIndexConfig::transparent_address(),
            value_balance: ZainoIndexConfig::value_balance(),
        }
    }
}

/// The wallet-facing gRPC server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServeConfig {
    /// Address the `CompactTxStreamer` gRPC server listens on.
    pub grpc_listen_address: SocketAddr,
    /// Most transparent receives one request may walk, across all its addresses. Over it the
    /// request is `RESOURCE_EXHAUSTED`, never a short list or a partial balance. Every address
    /// method walks the whole history, whatever height range it asks about.
    pub max_address_rows: NonZeroUsize,
    /// TLS on the gRPC listener. Absent, it serves plaintext HTTP/2 for a TLS-terminating proxy
    /// in front.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

/// PEM files the gRPC listener terminates TLS with (rustls + ring). Both are re-read within a
/// minute of changing, so a certificate renewal needs no restart; a pair that fails to load
/// keeps the previous one serving.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// Certificate chain, leaf first.
    pub cert_path: PathBuf,
    /// The certificate's private key (PKCS#8, PKCS#1 or SEC1).
    pub key_path: PathBuf,
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
pub struct GrpcConfig {
    /// Connections served at once; further accepts are closed. Each is a file descriptor, so
    /// zainod refuses to start when this does not fit its open-file limit.
    pub max_connections: NonZeroUsize,
    /// Connections one client may hold, so one wallet cannot take the whole cap. The client is
    /// the peer address, or behind a `trusted_proxies` entry the address its PROXY header names.
    pub max_connections_per_ip: NonZeroUsize,
    /// HTTP/2 `SETTINGS_MAX_CONCURRENT_STREAMS`, per connection.
    pub max_streams_per_connection: NonZeroU32,
    /// Work streams (every method but `GetMempoolStream`) admitted across every connection.
    pub max_streams: NonZeroUsize,
    /// `GetMempoolStream` subscriptions admitted across every connection. Separate from
    /// `max_streams`: a subscription idles until the next block, one per connected wallet.
    pub max_subscriptions: NonZeroUsize,
    /// Point reads (one block, one tree state, a subtree-root list) in flight on the blocking
    /// pool. Warm ones are CPU-bound, so about the core count.
    pub max_point_reads: NonZeroUsize,
    /// `GetBlockRange` windows (≤ 1 MiB each) in flight: the device queue depth.
    pub max_range_reads: NonZeroUsize,
    /// Transparent-address history scans in flight. Few: each walks an address's whole
    /// history (bounded per request by `serve.max_address_rows`).
    pub max_scan_reads: NonZeroUsize,
    /// Seconds a stream may hold data its client does not read before the connection is closed
    /// (a live client that never reads would otherwise keep its permits forever).
    pub stall_timeout_secs: NonZeroU64,
    /// Proxies (CIDRs) in front of this server that open every connection with a PROXY
    /// protocol header (v1 or v2) naming the real client. A connection from one of them without
    /// a header is closed. Empty = no proxy: the peer address is the client.
    pub trusted_proxies: Vec<ipnet::IpNet>,
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
        }
    }
}

/// The one block-fetch pipeline every index shares (sync = I/O bound on validator RPC).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct FetchConfig {
    /// Blocks below the tip kept reorg-able: in memory and served, written only once buried
    /// this deep. Default and minimum on mainnet and testnet: Zebra's reorg bound (1000); only
    /// regtest may set less.
    pub finalised_depth: NonZeroU32,
    /// Block fetches (and decodes) kept in flight during bulk sync.
    pub concurrency: NonZeroUsize,
    /// `jsonrpc_address` of the one validator bulk sync fetches from. Unset = spread across
    /// `source` and every `chainview_peers` entry.
    pub primary_validator: Option<String>,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            finalised_depth: NonZeroU32::new(MAX_BLOCK_REORG_HEIGHT)
                .expect("the consensus reorg bound is non-zero"),
            concurrency: NonZeroUsize::new(32).expect("32 is non-zero"),
            primary_validator: None,
        }
    }
}

/// The admin listener: `/metrics`, `/livez`, `/readyz` and `/statusz`. Disabled when
/// `listen_address` is unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Address the admin listener binds (`0.0.0.0` = every interface; who may reach it is the
    /// host's firewall).
    pub listen_address: Option<SocketAddr>,
}

/// Bootstrap an empty index from a published snapshot (`snapshot` feature, `aria2c` on PATH).
///
/// Only indexes whose directory is missing or empty are filled; one holding data is never touched.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotConfig {
    /// URL of the snapshot manifest: `{archive, bytes, sha256, height, network}` (`archive`
    /// relative to it).
    pub manifest: String,
    /// Parallel connections the archive downloads over (aria2c `--split`, at most 16).
    #[serde(default = "SnapshotConfig::connections_default")]
    pub connections: NonZeroU32,
}

impl SnapshotConfig {
    fn connections_default() -> NonZeroU32 {
        NonZeroU32::new(8).expect("8 is non-zero")
    }
}

/// The zainod daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// Chain this daemon serves: `mainnet`, `testnet` or `regtest`.
    ///
    /// Declared, never derived: Zebra on regtest reports its chain as `"test"` over
    /// `getblockchaininfo`, so a validator-derived value mislabels every regtest deployment.
    #[serde(with = "NetworkDef")]
    pub network: NetworkType,
    /// The admin listener (`/metrics` and the probes).
    pub metrics: MetricsConfig,
    /// The validator blocks are sourced from.
    pub source: SourceConfig,
    /// Extra validators the mempool view quorates over, beyond [`source`](Self::source).
    ///
    /// Empty is a one-validator deployment: the quorum is `source` alone, trivially met. Adding
    /// endpoints is what makes the view worth more than a wallet's own connection.
    #[serde(default)]
    pub chainview_peers: Vec<SourceConfig>,
    /// The wallet-facing gRPC server.
    pub serve: ServeConfig,
    /// What that server will serve at once.
    pub grpc: GrpcConfig,
    /// The shared block-fetch pipeline.
    pub fetch: FetchConfig,
    /// The indexes this daemon builds and serves.
    pub index: IndexConfig,
    /// Empty indexes bootstrapped from a snapshot. Absent = they sync from the validator.
    pub snapshot: Option<SnapshotConfig>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            // Mainnet = the deployment target; testnet/regtest operators declare theirs
            network: NetworkType::Main,
            metrics: MetricsConfig::default(),
            source: SourceConfig::default(),
            chainview_peers: Vec::new(),
            serve: ServeConfig::default(),
            grpc: GrpcConfig::default(),
            fetch: FetchConfig::default(),
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
    pub fn validate(&self) -> Result<(), IndexerError> {
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
        if let Some(primary) = &self.fetch.primary_validator {
            if self.primary_validator_index().is_none() {
                return Err(IndexerError::ConfigError(format!(
                    "fetch.primary_validator = {primary:?} names neither source nor a \
                     chainview_peers entry"
                )));
            }
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

    /// Every validator, `source` first (the order chainview and the fetch pool index them by)
    pub(crate) fn validators(&self) -> impl Iterator<Item = &SourceConfig> {
        std::iter::once(&self.source).chain(&self.chainview_peers)
    }

    /// `fetch.primary_validator`'s position in [`validators`](Self::validators)
    pub(crate) fn primary_validator_index(&self) -> Option<usize> {
        let primary = self.fetch.primary_validator.as_ref()?;
        self.validators().position(|validator| &validator.jsonrpc_address == primary)
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
pub fn generate_default_config() -> Result<String, IndexerError> {
    let toml = toml::to_string_pretty(&DaemonConfig::default())
        .map_err(|e| IndexerError::ConfigError(format!("serialising default config: {e}")))?;
    Ok(format!("{GENERATED_CONFIG_HEADER}{toml}"))
}

/// Load configuration from a TOML file with `ZAINO_CONFIG_` environment overrides.
pub fn load_config(file_path: &std::path::Path) -> Result<DaemonConfig, IndexerError> {
    load_config_with_env(file_path, "ZAINO_CONFIG")
}

/// Load configuration with a custom environment-variable prefix.
///
/// Layering: defaults → TOML file → environment (`<prefix>_`, `__` for nesting).
pub fn load_config_with_env(
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
        assert!(index.compact_block.path.ends_with("compact-block"));
        assert!(index.block_hash.path.ends_with("block-hash"));
        assert!(index.tree_state.path.ends_with("tree-state"));
        assert!(index.transparent_address.path.ends_with("transparent-address"));
        assert!(index.value_balance.path.ends_with("value-balance"));

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
        let base =
            "network = \"mainnet\"\n[source]\njsonrpc_address = \"127.0.0.1:8232\"\n[serve]\n";
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

    #[test]
    fn source_parses_with_auth_absent_and_removed_fields_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
[source]
jsonrpc_address = "127.0.0.1:18232"

[index.compact_block]
path = "/tmp/zaino-compact-block"
"#;
        let config = load_config(&write(&dir, "rpc.toml", toml)).expect("load");
        let address = "127.0.0.1:18232".to_string();
        let no_auth = SourceConfig { jsonrpc_address: address, ..SourceConfig::default() };
        assert_eq!(config.source, no_auth);
        assert_eq!(config.fetch, FetchConfig::default());

        for (name, stale_line) in [
            ("mode.toml", r#"mode = "direct""#),
            ("cache.toml", r#"zebra_cache_dir = "/var/lib/zebra""#),
        ] {
            let stale = toml.replace("[source]\n", &format!("[source]\n{stale_line}\n"));
            let err = load_config(&write(&dir, name, &stale)).expect_err(stale_line);
            assert!(err.to_string().contains("unknown field"), "{stale_line}: {err}");
        }
        let noderpc = format!("{toml}\n[serve]\njsonrpc_listen_address = \"0.0.0.0:8232\"\n");
        let err = load_config(&write(&dir, "noderpc.toml", &noderpc)).expect_err("noderpc");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn env_overrides_a_scalar_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = r#"
[source]
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
