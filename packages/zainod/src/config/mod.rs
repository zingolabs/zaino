//! zainod daemon config: the one operator surface
//!
//! - Table whose keys a runtime crate's own type spells = that type (`[grpc]` = [`GrpcLimits`],
//!   `[submission]` = [`SubmitPolicy`], `[sync.lsm_store]` = [`LsmConfig`])
//! - [`DaemonConfigFile`] + its `*Table`s = what the file holds, [`DaemonConfigFile::resolve`] →
//!   [`DaemonConfig`] (paths set, enabled indexes, endpoints: the only shape a run reads)
//! - Rest translated at boot ([`crate::daemon::start`])
//! - Layered highest-priority-first: env (`ZAINO_CONFIG_` prefix, `__` nesting) → TOML → defaults

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use zaino_chainview::SubmitPolicy;
use zaino_grpc::{GrpcLimits, TlsFiles};
use zaino_peers::PeerConfig;

use serde::{Deserialize, Serialize};
use tracing::info;

use zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS;
use zaino_persistence::{manifest_fits, LsmConfig};
use zaino_primitives::protocol::MAX_BLOCK_REORG_HEIGHT;
use zaino_primitives::serde_secs;
use zaino_source::{Auth, Endpoint, LinkLimits, ResponseSizeCap, Timeouts, ZebraIndexerTls};
use zaino_sync::IndexKind;
use zaino_traffic::{Limits, TrustedValidatorId};
use zcash_protocol::consensus::NetworkType;

use crate::error::IndexerError;

pub(super) mod paths;

/// One spelling for TOML, logs, `/status` and messages
pub(crate) fn network_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "mainnet",
        NetworkType::Test => "testnet",
        NetworkType::Regtest => "regtest",
    }
}

/// `network` as [`network_name`] (no serde impls upstream)
mod network_serde {
    use serde::de::{Error, Unexpected};
    use serde::{Deserialize, Deserializer, Serializer};
    use zcash_protocol::consensus::NetworkType;

    use super::network_name;

    pub(super) fn serialize<S: Serializer>(
        network: &NetworkType,
        to: S,
    ) -> Result<S::Ok, S::Error> {
        to.serialize_str(network_name(*network))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<NetworkType, D::Error> {
        let name = String::deserialize(from)?;
        let networks = [NetworkType::Main, NetworkType::Test, NetworkType::Regtest];
        let network = networks.into_iter().find(|network| network_name(*network) == name);
        network.ok_or_else(|| Error::invalid_value(Unexpected::Str(&name), &"a network name"))
    }
}

pub(crate) const GENERATED_CONFIG_HEADER: &str = r#"# Zaino daemon configuration
#
# Generated with `zainod generate-config`.
#
# Layered highest-priority-first: ZAINO_CONFIG_ env vars, then this file, then defaults.
# For documentation see https://github.com/zingolabs/zaino
"#;

/// `[[trusted_validators]]` entry: Zebra JSON-RPC endpoint Zaino trusts (headers for the verified
/// chain, mempool admission with fees, mined-transaction lookups)
///
/// - No vote (`priority` = who is asked first)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct TrustedValidatorTable {
    pub(crate) jsonrpc_address: String,
    cookie_path: Option<PathBuf>,
    user: Option<String>,
    password: Option<String>,
    #[serde(rename = "connect_timeout_secs", with = "serde_secs::nonzero")]
    connect_timeout: Duration,
    /// Silence, not total duration (a multi-MB block over a slow link takes as long as it takes)
    #[serde(rename = "read_timeout_secs", with = "serde_secs::nonzero")]
    read_timeout: Duration,
    /// The balancer's budget = the transport's pool (>= 5: poll + one per lane + one shared)
    max_connections: Limits,
    /// Largest JSON-RPC reply read: 4 (one full block) ..= 32 (batches split to fit)
    max_response_mib: ResponseMib,
    /// zebrad's `indexer_listen_addr` (set = push streams wake the poller, unset = poll each second)
    pub(crate) indexer_address: Option<String>,
    /// Unset = plaintext (zakurad requires it off loopback)
    indexer_tls: Option<ZebraIndexerTls>,
    /// Every 0 asked before any 1 (higher tier = overflow + hedges only)
    priority: u8,
}

impl Default for TrustedValidatorTable {
    fn default() -> Self {
        let timeouts = Timeouts::default();
        Self {
            jsonrpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
            connect_timeout: timeouts.connect,
            read_timeout: timeouts.read,
            max_connections: Limits::default(),
            max_response_mib: ResponseMib(LinkLimits::default().response_size_cap),
            indexer_address: None,
            indexer_tls: None,
            priority: 0,
        }
    }
}

impl TrustedValidatorTable {
    /// `Err` = credentials other than `cookie_path`, `user` + `password` together, or neither; or
    /// `indexer_tls` with no `indexer_address`
    fn resolve(self) -> Result<TrustedValidatorConfig, IndexerError> {
        let Self {
            jsonrpc_address: address,
            cookie_path,
            user,
            password,
            connect_timeout,
            read_timeout,
            max_connections,
            max_response_mib,
            indexer_address,
            indexer_tls,
            priority,
        } = self;
        let auth = match (cookie_path, user, password) {
            (None, None, None) => Auth::None,
            (Some(path), None, None) => Auth::Cookie(path),
            (None, Some(user), Some(password)) => Auth::Basic { user, password },
            (Some(_), _, _) => {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] {address} sets cookie_path and user/password: pick one"
                )))
            }
            _ => {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] {address}: user and password go together"
                )))
            }
        };
        let indexer = match (indexer_address, indexer_tls) {
            (None, Some(_)) => {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] {address} sets indexer_tls but no indexer_address"
                )))
            }
            (indexer, tls) => indexer.map(|address| ZebraIndexerConfig { address, tls }),
        };
        let timeouts = Timeouts { connect: connect_timeout, read: read_timeout };
        let limits = LinkLimits {
            max_connections: max_connections.max_connections(),
            response_size_cap: max_response_mib.0,
        };
        let endpoint = Endpoint { address, auth, timeouts, limits };
        Ok(TrustedValidatorConfig { endpoint, max_connections, indexer, priority })
    }
}

/// `max_response_mib`: refused at parse outside one full block's reply ..= 32 MiB
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u32", into = "u32")]
struct ResponseMib(ResponseSizeCap);

impl TryFrom<u32> for ResponseMib {
    type Error = String;

    fn try_from(mib: u32) -> Result<Self, String> {
        let bytes = NonZeroU32::new(mib).and_then(|mib| usize::try_from(bytes(mib).get()).ok());
        let cap = bytes.and_then(ResponseSizeCap::new);
        cap.map(Self).ok_or_else(|| format!("max_response_mib = {mib}: must be 4 ..= 32"))
    }
}

impl From<ResponseMib> for u32 {
    fn from(mib: ResponseMib) -> Self {
        u32::try_from(mib.0.bytes() >> 20).expect("ResponseSizeCap <= 32 MiB")
    }
}

/// Trusted validator, resolved: its JSON-RPC endpoint (address, auth, timeouts, link) + the
/// balancer's view of it
#[derive(Debug, Clone)]
pub(crate) struct TrustedValidatorConfig {
    pub(crate) endpoint: Endpoint,
    pub(crate) max_connections: Limits,
    pub(crate) indexer: Option<ZebraIndexerConfig>,
    pub(crate) priority: u8,
}

/// zebrad's indexer gRPC
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZebraIndexerConfig {
    pub(crate) address: String,
    pub(crate) tls: Option<ZebraIndexerTls>,
}

/// Enabled index, resolved: its directory + the shared `[sync]` queue budget (boot's input)
///
/// - Write buffer = the index crate's `WRITE_BUFFER`
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexConfig {
    pub(crate) kind: IndexKind,
    pub(crate) path: PathBuf,
    pub(crate) queue_bytes: NonZeroUsize,
}

/// Every enabled index, [`IndexKind::ALL`] order (>= 1, utxo_set beside compact_block, on with it)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnabledIndexes(Vec<IndexConfig>);

impl EnabledIndexes {
    pub(crate) fn get(&self, kind: IndexKind) -> Option<&IndexConfig> {
        self.0.iter().find(|index| index.kind == kind)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &IndexConfig> {
        self.0.iter()
    }
}

/// `[index.*]`: which indexes this daemon builds and serves, one directory each
///
/// - defaults name every path (`generate-config` prints them)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
struct IndexTables {
    compact_block: IndexTable,
    block_hash: IndexTable,
    tree_state: IndexTable,
    transparent_address: IndexTable,
    txid_location: IndexTable,
}

impl Default for IndexTables {
    fn default() -> Self {
        let table = |kind| IndexTable { path: paths::default_index(kind), enabled: true };
        Self {
            compact_block: table(IndexKind::CompactBlock),
            block_hash: table(IndexKind::BlockHash),
            tree_state: table(IndexKind::TreeState),
            transparent_address: table(IndexKind::TransparentAddress),
            txid_location: table(IndexKind::TxidLocation),
        }
    }
}

impl IndexTables {
    /// `Err` = an enabled table's unset path with no home dir, compact_block's path with no final
    /// name, or none enabled
    fn resolve(self, queue_mib: NonZeroU32) -> Result<EnabledIndexes, IndexerError> {
        let path = |table: IndexTable, kind: IndexKind| match table.enabled {
            false => Ok(None),
            true => {
                let key = format!("index.{}.path", kind.name());
                resolved_path(table.path, &key, || paths::default_index(kind)).map(Some)
            }
        };
        let compact_block = path(self.compact_block, IndexKind::CompactBlock)?;
        let block_hash = path(self.block_hash, IndexKind::BlockHash)?;
        let tree_state = path(self.tree_state, IndexKind::TreeState)?;
        let transparent_address = path(self.transparent_address, IndexKind::TransparentAddress)?;
        let txid_location = path(self.txid_location, IndexKind::TxidLocation)?;
        let utxo_set = match &compact_block {
            Some(beside) if beside.file_name().is_none() => {
                return Err(IndexerError::Config(format!(
                    "index.compact_block.path = {}: no final directory name (utxo_set sits \
                     beside it)",
                    beside.display()
                )))
            }
            beside => {
                beside.as_ref().map(|beside| beside.with_file_name(IndexKind::UtxoSet.name()))
            }
        };
        // past `usize` = no limit anyway
        let queue_bytes = NonZeroUsize::try_from(bytes(queue_mib)).unwrap_or(NonZeroUsize::MAX);
        let enabled: Vec<IndexConfig> = IndexKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let path = match kind {
                    IndexKind::CompactBlock => &compact_block,
                    IndexKind::UtxoSet => &utxo_set,
                    IndexKind::BlockHash => &block_hash,
                    IndexKind::TreeState => &tree_state,
                    IndexKind::TransparentAddress => &transparent_address,
                    IndexKind::TxidLocation => &txid_location,
                };
                path.clone().map(|path| IndexConfig { kind, path, queue_bytes })
            })
            .collect();
        if enabled.is_empty() {
            return Err(IndexerError::Config(
                "every [index.*] disabled: nothing to build or serve".to_string(),
            ));
        }
        Ok(EnabledIndexes(enabled))
    }
}

/// `[index.<name>]` table (`path` created if absent, `None` = `<cache dir>/zaino/indexes/<name>`)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
struct IndexTable {
    /// Off = its methods answer `UNIMPLEMENTED`
    enabled: bool,
    path: Option<PathBuf>,
}

impl Default for IndexTable {
    fn default() -> Self {
        Self { enabled: true, path: None }
    }
}

fn bytes(mib: NonZeroU32) -> NonZeroU64 {
    let mib_bytes = const { NonZeroU64::new(1 << 20).expect("2^20 is non-zero") };
    NonZeroU64::from(mib).saturating_mul(mib_bytes)
}

/// `[serve]`: wallet-facing gRPC server
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ServeTable {
    pub(crate) grpc_listen_address: SocketAddr,
    /// Across one request's addresses, whole history (over = `RESOURCE_EXHAUSTED`, never partial)
    max_address_rows: NonZeroUsize,
    /// Absent = plaintext HTTP/2 (for a TLS-terminating proxy)
    #[serde(skip_serializing_if = "Option::is_none")]
    tls: Option<TlsFiles>,
    /// Kept across restarts (`zainod hold` writes its marker here)
    pub(crate) state_dir: Option<PathBuf>,
    /// >= load balancer's mark-down time + DNS TTL
    #[serde(rename = "hold_goaway_delay_secs", with = "serde_secs::any")]
    hold_goaway_delay: Duration,
}

impl Default for ServeTable {
    fn default() -> Self {
        Self {
            grpc_listen_address: SocketAddr::from(([127, 0, 0, 1], 8137)),
            max_address_rows: DEFAULT_MAX_ADDRESS_ROWS,
            tls: None,
            state_dir: paths::default_state(),
            hold_goaway_delay: Duration::ZERO,
        }
    }
}

impl ServeTable {
    /// `Err` = unset `state_dir` with no home dir
    fn resolve(self) -> Result<ServeConfig, IndexerError> {
        let Self { grpc_listen_address, max_address_rows, tls, state_dir, hold_goaway_delay } =
            self;
        let state_dir = resolved_path(state_dir, "serve.state_dir", paths::default_state)?;
        Ok(ServeConfig { grpc_listen_address, max_address_rows, tls, state_dir, hold_goaway_delay })
    }
}

/// [`ServeTable`], resolved
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServeConfig {
    pub(crate) grpc_listen_address: SocketAddr,
    pub(crate) max_address_rows: NonZeroUsize,
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) state_dir: PathBuf,
    pub(crate) hold_goaway_delay: Duration,
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
    /// Per index: finalized blocks it may trail the fetch by before the pipeline throttles
    queue_mib: NonZeroU32,
    pub(crate) lsm_store: LsmConfig,
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
            lsm_store: LsmConfig::default(),
        }
    }
}

impl SyncConfig {
    /// `Err` = a fanout some enabled index's manifest can't hold, or finality inside the reorg
    /// bound off regtest
    fn check(&self, indexes: &EnabledIndexes, network: NetworkType) -> Result<(), IndexerError> {
        let fanout = self.lsm_store.fanout;
        for kind in indexes.iter().map(|index| index.kind) {
            if !manifest_fits(&zaino_nfs::schema(kind, network), fanout) {
                return Err(IndexerError::Config(format!(
                    "sync.lsm_store.fanout = {}: {}'s segment lists at that fanout outgrow its \
                     manifest (lower it)",
                    fanout.get(),
                    kind.name()
                )));
            }
        }
        let depth = self.finalised_depth.get();
        if network != NetworkType::Regtest && depth < MAX_BLOCK_REORG_HEIGHT {
            return Err(IndexerError::Config(format!(
                "sync.finalised_depth = {depth} is below the validator's reorg bound \
                 {MAX_BLOCK_REORG_HEIGHT}: a reorg it accepts could reach durable blocks \
                 (only regtest may set less)"
            )));
        }
        Ok(())
    }
}

/// `[p2p]`: Zaino's own p2p peers: headers + blocks (checked, never vouched), finality on their
/// word (> 5 netgroups, no difficulty collapse), the mempool quorum (each peer's listing polled),
/// submission relays
///
/// - Serves no peer (listener on loopback, no services advertised)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
struct P2pTable {
    /// Off = every submission attempt → a trusted validator
    enabled: bool,
    /// Mainnet peers allow one connection per IP (keep it modest)
    peer_target: NonZeroUsize,
    /// Empty = the network's DNS seeders (regtest has none)
    initial_peers: Vec<String>,
    /// zebra-network peer address cache (restart dials known peers, not only seeders)
    cache_dir: Option<PathBuf>,
}

impl Default for P2pTable {
    fn default() -> Self {
        Self {
            enabled: false,
            peer_target: zaino_peers::DEFAULT_PEER_TARGET,
            initial_peers: Vec::new(),
            cache_dir: paths::default_peer_cache(),
        }
    }
}

impl P2pTable {
    /// `None` = off (`Err` = on with no `cache_dir` home dir default, too few peers, or nobody to
    /// dial)
    fn resolve(self, network: NetworkType) -> Result<Option<PeerConfig>, IndexerError> {
        let Self { enabled, peer_target, initial_peers, cache_dir } = self;
        if !enabled {
            return Ok(None);
        }
        let cache_dir = resolved_path(cache_dir, "p2p.cache_dir", paths::default_peer_cache)?;
        let min = usize::try_from(zaino_traffic::Peers::MIN_IN_FLIGHT).expect("lane count");
        if peer_target.get() < min {
            return Err(IndexerError::Config(format!(
                "p2p.peer_target = {peer_target} is below {}: the peer set carries one request \
                 per lane at once (headers, tip blocks, bulk blocks)",
                zaino_traffic::Peers::MIN_IN_FLIGHT
            )));
        }
        if network == NetworkType::Regtest && initial_peers.is_empty() {
            return Err(IndexerError::Config(
                "p2p.enabled on regtest with no p2p.initial_peers: regtest has no DNS seeders"
                    .to_string(),
            ));
        }
        Ok(Some(PeerConfig {
            peer_target,
            initial_peers,
            cache_dir: Some(cache_dir),
            ..PeerConfig::new(network)
        }))
    }
}

/// Admin listener: `/metrics`, `/livez`, `/readyz` (unset `listen_address` = off)
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct MetricsConfig {
    /// `0.0.0.0` = every interface (access = the host's firewall)
    pub(crate) listen_address: Option<SocketAddr>,
}

/// Empty index ← published index snapshot (`aria2c` on PATH)
///
/// - Only a missing or empty directory filled
/// - Directory holding data never touched
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

    fn check(&self) -> Result<(), IndexerError> {
        if self.connections.get() > 16 {
            return Err(IndexerError::Config(format!(
                "snapshot.connections = {}: aria2c allows at most 16 per server",
                self.connections
            )));
        }
        Ok(())
    }
}

/// `ZcashLegacyRpcClient` or `ZebraIndexerGrpcClient`, one for every validator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidatorClient {
    LegacyRpc,
    Grpc,
}

/// What the TOML file (+ env) holds, before [`DaemonConfigFile::resolve`]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct DaemonConfigFile {
    /// Declared, never derived (regtest Zebra reports `"test"` over `getblockchaininfo`)
    #[serde(with = "network_serde")]
    pub(crate) network: NetworkType,
    /// `grpc` = blocks over each validator's `indexer_address` (zcashd has none: `legacy_rpc`)
    validator_client: ValidatorClient,
    metrics: MetricsConfig,
    /// >= 1 (two = first set surviving a failure)
    pub(crate) trusted_validators: Vec<TrustedValidatorTable>,
    pub(crate) serve: ServeTable,
    pub(crate) grpc: GrpcLimits,
    submission: SubmitPolicy,
    p2p: P2pTable,
    pub(crate) sync: SyncConfig,
    index: IndexTables,
    /// Absent = synced from the validator
    snapshot: Option<IndexSnapshotConfig>,
}

impl Default for DaemonConfigFile {
    fn default() -> Self {
        Self {
            // Mainnet = the deployment target (testnet/regtest operators declare theirs)
            network: NetworkType::Main,
            // every validator speaks it (zcashd has no indexer gRPC)
            validator_client: ValidatorClient::LegacyRpc,
            metrics: MetricsConfig::default(),
            trusted_validators: vec![TrustedValidatorTable::default()],
            serve: ServeTable::default(),
            grpc: GrpcLimits::default(),
            submission: SubmitPolicy::default(),
            p2p: P2pTable::default(),
            sync: SyncConfig::default(),
            index: IndexTables::default(),
            snapshot: None,
        }
    }
}

impl DaemonConfigFile {
    /// `Err` = a path with no home dir default (names its key), or a config no pipeline composes
    /// from
    pub(crate) fn resolve(self) -> Result<DaemonConfig, IndexerError> {
        let Self {
            network,
            validator_client,
            metrics,
            trusted_validators,
            serve,
            grpc,
            submission,
            p2p,
            sync,
            index,
            snapshot,
        } = self;
        let serve = serve.resolve()?;
        let indexes = index.resolve(sync.queue_mib)?;
        sync.check(&indexes, network)?;
        let trusted_validators = resolve_validators(trusted_validators)?;
        let p2p = p2p.resolve(network)?;
        snapshot.as_ref().map_or(Ok(()), IndexSnapshotConfig::check)?;
        Ok(DaemonConfig {
            network,
            validator_client,
            metrics,
            trusted_validators,
            serve,
            grpc,
            submission,
            p2p,
            sync,
            indexes,
            snapshot,
        })
    }
}

/// `Err` = none, more than the balancer's ids, one address twice, or an entry
/// [`TrustedValidatorTable::resolve`] refuses
fn resolve_validators(
    tables: Vec<TrustedValidatorTable>,
) -> Result<Vec<TrustedValidatorConfig>, IndexerError> {
    let count = tables.len();
    if count == 0 {
        return Err(IndexerError::Config(
            "no [[trusted_validators]]: at least one validator is needed".to_string(),
        ));
    }
    if count > TrustedValidatorId::MAX {
        return Err(IndexerError::Config(format!(
            "{count} [[trusted_validators]]: at most {} are supported",
            TrustedValidatorId::MAX
        )));
    }
    let mut addresses = std::collections::HashSet::new();
    let resolve = |table: TrustedValidatorTable| {
        if !addresses.insert(table.jsonrpc_address.clone()) {
            return Err(IndexerError::Config(format!(
                "[[trusted_validators]] lists {} twice: one validator counted as two",
                table.jsonrpc_address
            )));
        }
        table.resolve()
    };
    tables.into_iter().map(resolve).collect()
}

/// [`DaemonConfigFile`], resolved: what the daemon, `verify`, `hold` and `overload` run on
#[derive(Debug)]
pub(crate) struct DaemonConfig {
    pub(crate) network: NetworkType,
    pub(crate) validator_client: ValidatorClient,
    pub(crate) metrics: MetricsConfig,
    pub(crate) trusted_validators: Vec<TrustedValidatorConfig>,
    pub(crate) serve: ServeConfig,
    pub(crate) grpc: GrpcLimits,
    pub(crate) submission: SubmitPolicy,
    pub(crate) p2p: Option<PeerConfig>,
    pub(crate) sync: SyncConfig,
    pub(crate) indexes: EnabledIndexes,
    pub(crate) snapshot: Option<IndexSnapshotConfig>,
}

impl DaemonConfig {
    pub(crate) fn warn_about_metrics_listener(&self) {
        let Some(endpoint) = self.metrics.listen_address else {
            return;
        };
        // - Public bind publishes chain tip, sync progress, request volumes, RSS
        // - Warn, not reject (read-only telemetry, containers bind 0.0.0.0 by norm)
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

/// `path`, else its home-dir `default` (`Err` = neither: names `key`)
fn resolved_path(
    path: Option<PathBuf>,
    key: &str,
    default: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf, IndexerError> {
    let missing =
        || IndexerError::Config(format!("{key}: no home directory to default it, set it"));
    path.or_else(default).ok_or_else(missing)
}

fn is_private_listen_addr(addr: &SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ipv4) => ipv4.is_private() || ipv4.is_loopback(),
        std::net::IpAddr::V6(ipv6) => ipv6.is_unique_local() || ipv6.is_loopback(),
    }
}

/// Defaults as TOML, behind [`GENERATED_CONFIG_HEADER`] (`Err` = defaults that don't resolve: a
/// path with no home dir default)
fn generate_default_config() -> Result<String, IndexerError> {
    let defaults = DaemonConfigFile::default();
    defaults.clone().resolve()?;
    let toml =
        toml::to_string_pretty(&defaults).expect("default DaemonConfigFile serializes to TOML");
    Ok(format!("{GENERATED_CONFIG_HEADER}{toml}"))
}

/// `zainod generate-config`: defaults → `path` (parent directories created)
pub(super) fn generate_config(path: &Path) -> Result<ExitCode, IndexerError> {
    let content = generate_default_config()?;
    let failed = |source| IndexerError::ConfigFile { path: path.to_owned(), source };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    std::fs::write(path, content).map_err(failed)?;
    eprintln!("Generated config file: {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Defaults → `file_path` (TOML) → env (`ZAINO_CONFIG_`, `__` nesting), then resolved
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

    let parsed: DaemonConfigFile = settings
        .try_deserialize()
        .map_err(|e| IndexerError::Config(format!("parsing configuration: {e}")))?;
    let resolved = parsed.resolve()?;

    info!(path = %file_path.display(), "Config loaded");
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use zaino_persistence::Fanout;

    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).expect("write config");
        path
    }

    /// Per index: file over defaults, env over file, `[sync]` budgets in each, utxo_set
    /// beside compact_block, every one in fold order
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
        assert_eq!(config.sync, sync, "[sync]: file's depth, env's queue_mib");
        let at = |kind, path: PathBuf| {
            let queue_bytes = NonZeroUsize::new(128 << 20).expect("non-zero");
            IndexConfig { kind, path, queue_bytes }
        };
        let block_hash = paths::default_index(IndexKind::BlockHash).expect("a home dir");
        let txid_location = paths::default_index(IndexKind::TxidLocation).expect("a home dir");
        let expected = EnabledIndexes(vec![
            at(IndexKind::UtxoSet, "/srv/zaino/utxo_set".into()),
            at(IndexKind::CompactBlock, "/srv/zaino/compact_block".into()),
            at(IndexKind::BlockHash, block_hash),
            at(IndexKind::TxidLocation, txid_location),
        ]);
        assert_eq!(
            config.indexes, expected,
            "utxo_set beside the file's compact_block, defaults for the unset, tree_state off by \
             env over its file path, transparent_address off by file"
        );
    }

    /// No home dir: an unset path = error naming its key, a set one kept
    #[test]
    fn a_path_with_no_home_dir_default_must_be_set() {
        let refused = resolved_path(None, "serve.state_dir", || None).map_err(|e| e.to_string());
        assert!(
            refused.as_ref().is_err_and(|refused| refused.contains("serve.state_dir")),
            "unset: refused naming the key, got {refused:?}"
        );

        let set = resolved_path(Some("/srv/zaino/state".into()), "serve.state_dir", || None);
        let set = set.map_err(|e| e.to_string());
        assert_eq!(set, Ok(PathBuf::from("/srv/zaino/state")), "set path kept");
    }

    #[test]
    fn utxo_set_is_off_with_compact_block() {
        let file: DaemonConfigFile =
            toml::from_str("[index.compact_block]\nenabled = false\n").expect("parses");
        let indexes = file.resolve().expect("resolves").indexes;
        assert_eq!(indexes.get(IndexKind::CompactBlock), None, "compact_block off");
        assert_eq!(indexes.get(IndexKind::UtxoSet), None, "utxo_set off with it");
    }

    /// Each row = one rule over an otherwise valid default config (`None` = accepted)
    #[test]
    fn resolve_refuses_every_config_no_pipeline_composes_from() {
        type Edit = fn(&mut DaemonConfigFile);
        fn validators(count: u16) -> Vec<TrustedValidatorTable> {
            let at = |port| format!("10.0.0.1:{port}");
            let validator = |port| TrustedValidatorTable {
                jsonrpc_address: at(port),
                ..TrustedValidatorTable::default()
            };
            (0..count).map(validator).collect()
        }
        // case, edit over the defaults, refusal names (None = accepted)
        let cases: [(&str, Edit, Option<&str>); 17] = [
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
                    c.index.txid_location.enabled = false;
                },
                Some("every [index.*] disabled"),
            ),
            (
                "no sibling for utxo_set",
                |c| c.index.compact_block.path = Some("/".into()),
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
                |c| c.trusted_validators.push(TrustedValidatorTable::default()),
                Some("lists 127.0.0.1:8232 twice"),
            ),
            (
                "64 validators, the balancer's every member id",
                |c| c.trusted_validators = validators(64),
                None,
            ),
            (
                "65 validators, past TrustedValidatorId::MAX",
                |c| c.trusted_validators = validators(65),
                Some("65 [[trusted_validators]]: at most 64 are supported"),
            ),
            (
                "indexer TLS with no indexer",
                |c| {
                    c.trusted_validators[0].indexer_tls = Some(ZebraIndexerTls {
                        ca_path: "/ca.pem".into(),
                        cert_path: "/cert.pem".into(),
                        key_path: "/key.pem".into(),
                        server_name: None,
                    })
                },
                Some("sets indexer_tls but no indexer_address"),
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
                Some("snapshot.connections = 17: aria2c allows at most 16"),
            ),
            (
                "p2p off ignores its peer target",
                |c| c.p2p.peer_target = NonZeroUsize::new(2).expect("2"),
                None,
            ),
        ];
        for (case, edit, refusal) in cases {
            let mut file = DaemonConfigFile::default();
            edit(&mut file);
            let refused = file.resolve().err().map(|error| error.to_string());
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
            let refused = toml::from_str::<DaemonConfigFile>(&toml).expect_err(table).to_string();
            assert!(refused.contains("unknown field `bogus`"), "{table}: {refused}");
        }
    }

    /// - `generate-config` = every key at its default, parsing back to the defaults (indexes
    ///   under `<cache dir>/zaino/indexes/<name>`), written under a directory it creates
    /// - Shipped example resolves, its values = the defaults (beside its network, validator, paths)
    #[test]
    fn generated_config_prints_every_key_at_its_default_and_the_example_shows_them() {
        let generated = generate_default_config().expect("a home dir");
        let body = generated.strip_prefix(GENERATED_CONFIG_HEADER).expect("header first");
        let defaults = DaemonConfigFile::default();
        let parsed = toml::from_str::<DaemonConfigFile>(body).expect("parses back");
        assert_eq!(parsed, defaults, "generated config parsed back = DaemonConfigFile::default()");
        let printed = |table: &str, keys: &[&str]| {
            let start = body.find(&format!("\n[{table}]\n")).expect(table);
            let section = body[start + 1..].split("\n\n").next().unwrap_or_default();
            let found: Vec<&str> =
                section.lines().skip(1).filter_map(|l| l.split(" = ").next()).collect();
            assert_eq!(found, keys, "[{table}]");
        };
        printed("sync", &["finalised_depth", "concurrency", "queue_mib"]);
        printed("sync.lsm_store", &["fanout", "merge_slots", "merge_mib_per_sec"]);
        for table in
            ["compact_block", "block_hash", "tree_state", "transparent_address", "txid_location"]
        {
            printed(&format!("index.{table}"), &["enabled", "path"]);
        }
        assert!(!body.contains("utxo_set"), "internal: no table of its own");
        let resolved = defaults.clone().resolve().expect("defaults resolve");
        for kind in [IndexKind::CompactBlock, IndexKind::UtxoSet, IndexKind::TreeState] {
            let path = resolved.indexes.get(kind).map(|index| index.path.clone());
            assert_eq!(path, paths::default_index(kind), "{kind:?}");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new/zainod.toml");
        assert_eq!(generate_config(&path).expect("writes"), ExitCode::SUCCESS, "into new/");
        let written = std::fs::read_to_string(&path).expect("written");
        assert_eq!(written, generated, "{} = the generated config", path.display());

        let example = include_str!("../../../../docs/example_configs/zainod.toml");
        let example = toml::from_str::<DaemonConfigFile>(example).expect("example parses");
        let resolved = example.clone().resolve().map(drop).map_err(|error| error.to_string());
        assert_eq!(resolved, Ok(()), "shipped example resolves");
        let shown = DaemonConfigFile {
            network: defaults.network,
            trusted_validators: defaults.trusted_validators.clone(),
            index: defaults.index.clone(),
            ..example
        };
        assert_eq!(shown, defaults, "example's values drifted from DaemonConfigFile::default()");
    }

    /// Every table but the indexes and validators (their own tests) → the struct its consumer
    /// runs: gRPC server, hold, TLS, validator client, LSM engine, submission, peer set
    ///
    /// - a named key keeps the consumer's own default for every other (`..Default::default()`)
    #[test]
    fn one_toml_reaches_every_consumer_as_written() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
network = "testnet"
validator_client = "grpc"

[serve]
hold_goaway_delay_secs = 155

[serve.tls]
cert_path = "/c.pem"
key_path = "/k.pem"

[grpc]
max_work_streams = 64
unread_timeout_secs = 30
silent_timeout_secs = 45
max_connection_age_secs = 0
max_connection_age_grace_secs = 60
max_connection_idle_secs = 120
adaptive_work_streams = false
shutdown_grace_secs = 600
trusted_proxies = ["10.0.0.0/8"]

[sync.lsm_store]
fanout = 4
merge_slots = 1
merge_mib_per_sec = 300

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
        let config = file.resolve().expect("resolves");
        let secs = Duration::from_secs;

        let grpc = GrpcLimits {
            max_work_streams: NonZeroUsize::new(64).expect("non-zero"),
            unread_timeout: secs(30),
            silent_timeout: secs(45),
            max_connection_age: None,
            max_connection_age_grace: secs(60),
            max_connection_idle: Some(secs(120)),
            adaptive_work_streams: false,
            drain_timeout: secs(600),
            trusted_proxies: zaino_grpc::TrustedProxies::new(vec!["10.0.0.0/8"
                .parse()
                .expect("net")]),
            ..GrpcLimits::default()
        };
        assert_eq!(config.grpc, grpc, "[grpc] (age 0 = never, shutdown grace = drain)");
        assert_eq!(config.serve.hold_goaway_delay, secs(155), "[serve] hold delay");
        let tls = TlsFiles { cert_path: "/c.pem".into(), key_path: "/k.pem".into() };
        assert_eq!(config.serve.tls, Some(tls), "[serve.tls] PEM pair");
        assert_eq!(config.validator_client, ValidatorClient::Grpc, "validator_client");
        let lsm = LsmConfig {
            fanout: Fanout::new(4).expect(">= 2"),
            merge_slots: NonZeroUsize::MIN,
            merge_mib_per_sec: NonZeroU32::new(300).expect("non-zero"),
        };
        assert_eq!(config.sync.lsm_store, lsm, "[sync.lsm_store] = the engine's LsmConfig");
        let submission = SubmitPolicy {
            propagation_threshold: secs(30),
            max_attempts: std::num::NonZeroU8::new(6).expect("non-zero"),
        };
        assert_eq!(config.submission, submission, "[submission]");
        let peers = PeerConfig {
            peer_target: NonZeroUsize::new(12).expect("non-zero"),
            initial_peers: vec!["203.0.113.7:18233".to_owned()],
            cache_dir: Some(PathBuf::from("/var/cache/zaino/peers")),
            ..PeerConfig::new(NetworkType::Test)
        };
        assert_eq!(config.p2p, Some(peers), "[p2p] on the declared network");
    }

    /// A value no consumer can run is refused at parse, the message naming key and rule
    #[test]
    fn a_value_no_consumer_can_run_is_refused_at_parse_naming_it() {
        #[rustfmt::skip]
        let refusals = [
            // case,                           toml,                                              refusal names
            ("fanout below a merge's 2",       "[sync.lsm_store]\nfanout = 1",                    ["fanout = 1", "a merge needs 2 segments"]),
            ("reply cap under one block",      "[[trusted_validators]]\nmax_response_mib = 3",    ["max_response_mib = 3", "must be 4 ..= 32"]),
            ("reply cap past 32 MiB",          "[[trusted_validators]]\nmax_response_mib = 33",   ["max_response_mib = 33", "must be 4 ..= 32"]),
            ("pool under one per lane",        "[[trusted_validators]]\nmax_connections = 4",     ["max_connections = 4", "is below 5"]),
        ];
        for (case, toml, named) in refusals {
            let refused =
                toml::from_str::<DaemonConfigFile>(toml).map_err(|error| error.to_string());
            for name in named {
                assert!(
                    refused.as_ref().is_err_and(|refused| refused.contains(name)),
                    "{case}: refused naming {name:?}, got {refused:#?}"
                );
            }
        }
    }

    /// Entries parsed as written (auth + limits + indexer mutual TLS optional), each resolved →
    /// the transport's endpoint (auth, timeouts, link), the balancer's limits, its indexer
    #[test]
    fn trusted_validators_parse_and_resolve_into_endpoint_limits_and_indexer() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:18232"

[[trusted_validators]]
jsonrpc_address = "zebra-eu:8232"
user = "zaino"
password = "secret"
max_connections = 8
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
        let local = TrustedValidatorTable {
            jsonrpc_address: "127.0.0.1:18232".to_owned(),
            ..TrustedValidatorTable::default()
        };
        let indexer_tls = ZebraIndexerTls {
            ca_path: "/etc/zaino/indexer-ca.pem".into(),
            cert_path: "/etc/zaino/indexer-client.pem".into(),
            key_path: "/etc/zaino/indexer-client-key.pem".into(),
            server_name: None,
        };
        let eu = TrustedValidatorTable {
            jsonrpc_address: "zebra-eu:8232".to_owned(),
            user: Some("zaino".to_owned()),
            password: Some("secret".to_owned()),
            max_connections: Limits::new(8).expect("8 ≥ MIN_CONNECTIONS"),
            max_response_mib: ResponseMib::try_from(8).expect("in bounds"),
            indexer_address: Some("zebra-eu:8230".to_owned()),
            indexer_tls: Some(indexer_tls.clone()),
            priority: 1,
            ..TrustedValidatorTable::default()
        };
        assert_eq!(file.trusted_validators, [local.clone(), eu.clone()], "both entries");

        let resolved = eu.clone().resolve().expect("a user + password pair");
        let endpoint = &resolved.endpoint;
        let basic = Auth::Basic { user: "zaino".to_owned(), password: "secret".to_owned() };
        let timeouts = Timeouts { connect: Duration::from_secs(2), read: Duration::from_secs(30) };
        let link = LinkLimits {
            max_connections: n(8),
            response_size_cap: ResponseSizeCap::new(8 << 20).expect("in bounds"),
        };
        assert_eq!(
            (endpoint.address.as_str(), &endpoint.auth, endpoint.timeouts, endpoint.limits),
            ("zebra-eu:8232", &basic, timeouts, link),
            "eu's endpoint: user + password, default timeouts, its link"
        );
        let indexer =
            ZebraIndexerConfig { address: "zebra-eu:8230".to_owned(), tls: Some(indexer_tls) };
        assert_eq!(
            (resolved.max_connections, resolved.indexer, resolved.priority),
            (Limits::new(8).expect("8 ≥ MIN_CONNECTIONS"), Some(indexer), 1),
            "eu's balancer limits, indexer, tier"
        );
        let local = local.resolve().expect("no credentials");
        assert_eq!((local.endpoint.auth, local.indexer), (Auth::None, None), "local: neither");

        let cookie = |user: Option<&str>, password: Option<&str>| TrustedValidatorTable {
            cookie_path: Some("/zebra/.cookie".into()),
            user: user.map(str::to_owned),
            password: password.map(str::to_owned),
            ..TrustedValidatorTable::default()
        };
        let cookie_auth = cookie(None, None).resolve().expect("a cookie").endpoint.auth;
        assert_eq!(cookie_auth, Auth::Cookie("/zebra/.cookie".into()), "cookie_path alone");
        let half = TrustedValidatorTable { password: None, ..eu };
        let mixes = [
            ("cookie + pair", cookie(Some("u"), Some("p")), "sets cookie_path and user/password"),
            ("half pair", half, "zebra-eu:8232: user and password go together"),
        ];
        for (case, table, named) in mixes {
            let refused = table.resolve().map(drop).map_err(|error| error.to_string());
            assert!(
                refused.as_ref().is_err_and(|refused| refused.contains(named)),
                "{case}: refused naming {named:?}, got {refused:?}"
            );
        }
    }
}
