//! zainod daemon config: the one operator surface
//!
//! - Table whose keys a runtime crate's own type spells = that type (`[grpc]` = [`GrpcLimits`],
//!   `[submission]` = [`SubmitPolicy`], `[sync.lsm_store]` = [`LsmConfig`])
//! - Rest translated at boot ([`crate::daemon::start`])
//! - Layered highest-priority-first: env (`ZAINO_CONFIG_` prefix, `__` nesting) → TOML → defaults

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use zaino_chainview::SubmitPolicy;
use zaino_grpc::{GrpcLimits, TlsFiles};

use serde::{Deserialize, Serialize};
use tracing::info;

use zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS;
use zaino_persistence::{manifest_fits, IndexKind, LsmConfig};
use zaino_primitives::protocol::MAX_BLOCK_REORG_HEIGHT;
use zaino_primitives::serde_secs;
use zaino_source::{Auth, Endpoint, LinkLimits, ResponseSizeCap, Timeouts, ZebraIndexerTls};
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

/// Zebra JSON-RPC endpoint Zaino trusts: headers for the verified chain, mempool admission (with
/// fees), mined-transaction lookups
///
/// - No vote (`priority` = who is asked first)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct TrustedValidatorConfig {
    pub(crate) jsonrpc_address: String,
    pub(crate) cookie_path: Option<PathBuf>,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    #[serde(rename = "connect_timeout_secs", with = "serde_secs::nonzero")]
    pub(crate) connect_timeout: Duration,
    /// Silence, not total duration (a multi-MB block over a slow link takes as long as it takes)
    #[serde(rename = "read_timeout_secs", with = "serde_secs::nonzero")]
    pub(crate) read_timeout: Duration,
    /// The balancer's budget = the transport's pool (>= 5: poll + one per lane + one shared)
    pub(crate) max_connections: Limits,
    /// Largest JSON-RPC reply read: 4 (one full block) ..= 32 (batches split to fit)
    pub(crate) max_response_mib: ResponseMib,
    /// zebrad's `indexer_listen_addr` (set = push streams wake the poller, unset = poll each second)
    pub(crate) indexer_address: Option<String>,
    /// Unset = plaintext (zakurad requires it off loopback)
    pub(crate) indexer_tls: Option<ZebraIndexerTls>,
    /// Every 0 asked before any 1 (higher tier = overflow + hedges only)
    pub(crate) priority: u8,
}

impl Default for TrustedValidatorConfig {
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

/// `max_response_mib`: refused at parse outside one full block's reply ..= 32 MiB
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u32", into = "u32")]
pub(crate) struct ResponseMib(ResponseSizeCap);

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
        (mib.0.bytes() >> 20) as u32
    }
}

impl TrustedValidatorConfig {
    /// The transport's: connection pool + reply cap
    pub(crate) fn link(&self) -> LinkLimits {
        LinkLimits {
            max_connections: self.max_connections.max_connections(),
            response_size_cap: self.max_response_mib.0,
        }
    }

    pub(crate) fn timeouts(&self) -> Timeouts {
        Timeouts { connect: self.connect_timeout, read: self.read_timeout }
    }

    /// `cookie_path`, or `user` + `password` together, or neither (`Err` = any other mix)
    pub(crate) fn auth(&self) -> Result<Auth, IndexerError> {
        let address = &self.jsonrpc_address;
        match (&self.cookie_path, &self.user, &self.password) {
            (None, None, None) => Ok(Auth::None),
            (Some(path), None, None) => Ok(Auth::Cookie(path.clone())),
            (None, Some(user), Some(password)) => {
                Ok(Auth::Basic { user: user.clone(), password: password.clone() })
            }
            (Some(_), _, _) => Err(IndexerError::Config(format!(
                "[[trusted_validators]] {address} sets cookie_path and user/password: pick one"
            ))),
            _ => Err(IndexerError::Config(format!(
                "[[trusted_validators]] {address}: user and password go together"
            ))),
        }
    }

    pub(crate) fn endpoint(&self) -> Result<Endpoint, IndexerError> {
        let (address, auth) = (self.jsonrpc_address.clone(), self.auth()?);
        Ok(Endpoint { address, auth, timeouts: self.timeouts(), limits: self.link() })
    }
}

/// Enabled index, resolved: its directory + the shared `[sync]` queue budget (boot's input)
///
/// - Write buffer = the index crate's `WRITE_BUFFER`
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexConfig {
    pub(crate) path: PathBuf,
    pub(crate) queue_bytes: NonZeroUsize,
}

/// `[index.*]`: which indexes this daemon builds and serves, one directory each
///
/// - read through [`DaemonConfig::enabled`] (resolves `[sync]` in, and utxo_set)
/// - defaults name every path (`generate-config` prints them)
/// - table without one = `None`
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Indexes {
    compact_block: IndexTable,
    block_hash: IndexTable,
    tree_state: IndexTable,
    transparent_address: IndexTable,
    txid_location: IndexTable,
}

impl Default for Indexes {
    fn default() -> Self {
        let table = |kind| IndexTable { path: Some(paths::default_index(kind)), enabled: true };
        Self {
            compact_block: table(IndexKind::CompactBlock),
            block_hash: table(IndexKind::BlockHash),
            tree_state: table(IndexKind::TreeState),
            transparent_address: table(IndexKind::TransparentAddress),
            txid_location: table(IndexKind::TxidLocation),
        }
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

/// A MiB knob in bytes
fn bytes(mib: NonZeroU32) -> NonZeroU64 {
    let mib_bytes = const { NonZeroU64::new(1 << 20).expect("2^20 is non-zero") };
    NonZeroU64::from(mib).saturating_mul(mib_bytes)
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
    pub(crate) tls: Option<TlsFiles>,
    /// Kept across restarts (`zainod hold` writes its marker here)
    pub(crate) state_dir: PathBuf,
    /// >= load balancer's mark-down time + DNS TTL
    #[serde(rename = "hold_goaway_delay_secs", with = "serde_secs::any")]
    pub(crate) hold_goaway_delay: Duration,
}

impl Default for ServeConfig {
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
            cache_dir: paths::default_peer_cache(),
        }
    }
}

impl P2pConfig {
    pub(crate) fn peers(&self, network: NetworkType) -> zaino_peers::PeerConfig {
        zaino_peers::PeerConfig {
            peer_target: self.peer_target,
            initial_peers: self.initial_peers.clone(),
            cache_dir: Some(self.cache_dir.clone()),
            ..zaino_peers::PeerConfig::new(network)
        }
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
    #[serde(with = "network_serde")]
    pub(crate) network: NetworkType,
    /// `grpc` = blocks over each validator's `indexer_address` (zcashd has none: `legacy_rpc`)
    pub(crate) validator_client: ValidatorClient,
    pub(crate) metrics: MetricsConfig,
    /// >= 1 (two = first set surviving a failure)
    pub(crate) trusted_validators: Vec<TrustedValidatorConfig>,
    pub(crate) serve: ServeConfig,
    pub(crate) grpc: GrpcLimits,
    pub(crate) submission: SubmitPolicy,
    pub(crate) p2p: P2pConfig,
    pub(crate) sync: SyncConfig,
    pub(crate) index: Indexes,
    /// Absent = synced from the validator
    pub(crate) snapshot: Option<IndexSnapshotConfig>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            // Mainnet = the deployment target (testnet/regtest operators declare theirs)
            network: NetworkType::Main,
            // every validator speaks it (zcashd has no indexer gRPC)
            validator_client: ValidatorClient::LegacyRpc,
            metrics: MetricsConfig::default(),
            trusted_validators: vec![TrustedValidatorConfig::default()],
            serve: ServeConfig::default(),
            grpc: GrpcLimits::default(),
            submission: SubmitPolicy::default(),
            p2p: P2pConfig::default(),
            sync: SyncConfig::default(),
            index: Indexes::default(),
            snapshot: None,
        }
    }
}

impl DaemonConfig {
    /// `kind`'s directory + the `[sync]` budgets (`None` = disabled)
    ///
    /// - utxo_set = compact_block's fee index: on with it, `utxo_set` beside its `path`
    pub(crate) fn enabled(&self, kind: IndexKind) -> Option<IndexConfig> {
        let index = &self.index;
        let table = |table: &IndexTable, of: IndexKind| {
            let path = table.path.clone().unwrap_or_else(|| paths::default_index(of));
            (table.enabled, path)
        };
        let (enabled, path) = match kind {
            IndexKind::CompactBlock => table(&index.compact_block, kind),
            IndexKind::UtxoSet => {
                let (enabled, beside) = table(&index.compact_block, IndexKind::CompactBlock);
                (enabled, beside.with_file_name(kind.name()))
            }
            IndexKind::BlockHash => table(&index.block_hash, kind),
            IndexKind::TreeState => table(&index.tree_state, kind),
            IndexKind::TransparentAddress => table(&index.transparent_address, kind),
            IndexKind::TxidLocation => table(&index.txid_location, kind),
        };
        // past `usize` = no limit anyway
        let queue_bytes = NonZeroUsize::try_from(bytes(self.sync.queue_mib));
        let queue_bytes = queue_bytes.unwrap_or(NonZeroUsize::MAX);
        enabled.then_some(IndexConfig { path, queue_bytes })
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
        if IndexKind::ALL.iter().all(|kind| self.enabled(*kind).is_none()) {
            return Err(IndexerError::Config(
                "every [index.*] disabled: nothing to build or serve".to_string(),
            ));
        }
        let fanout = self.sync.lsm_store.fanout;
        for kind in IndexKind::ALL.into_iter().filter(|kind| self.enabled(*kind).is_some()) {
            if !manifest_fits(&zaino_nfs::schema(kind, self.network), fanout) {
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
                 {MAX_BLOCK_REORG_HEIGHT}: a reorg it accepts could reach durable blocks \
                 (only regtest may set less)"
            )));
        }
        Ok(())
    }

    fn validate_validators(&self) -> Result<(), IndexerError> {
        let count = self.trusted_validators.len();
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
        for validator in &self.trusted_validators {
            let address = &validator.jsonrpc_address;
            if !addresses.insert(address) {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] lists {address} twice: one validator counted as two"
                )));
            }
            validator.auth()?;
            if validator.indexer_tls.is_some() && validator.indexer_address.is_none() {
                return Err(IndexerError::Config(format!(
                    "[[trusted_validators]] {address} sets indexer_tls but no indexer_address"
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
    use zaino_persistence::Fanout;

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
        assert_eq!(config.sync, sync, "[sync]: file's depth, env's queue_mib");
        let at = |path: PathBuf| {
            Some(IndexConfig { path, queue_bytes: NonZeroUsize::new(128 << 20).expect("non-zero") })
        };
        let indexes = [
            (
                "compact_block: file's path",
                IndexKind::CompactBlock,
                at("/srv/zaino/compact_block".into()),
            ),
            (
                "utxo_set: beside compact_block",
                IndexKind::UtxoSet,
                at("/srv/zaino/utxo_set".into()),
            ),
            (
                "block_hash: default path",
                IndexKind::BlockHash,
                at(paths::default_index(IndexKind::BlockHash)),
            ),
            ("tree_state: env disabled over the file's path", IndexKind::TreeState, None),
            ("transparent_address: file disabled", IndexKind::TransparentAddress, None),
        ];
        for (case, kind, expected) in indexes {
            assert_eq!(config.enabled(kind), expected, "{case}");
        }
        let pair = config.compact_block().expect("a valid path");
        let expected = at("/srv/zaino/compact_block".into()).zip(at("/srv/zaino/utxo_set".into()));
        assert_eq!(pair, expected, "compact_block() = (compact_block, utxo_set)");
    }

    #[test]
    fn utxo_set_is_off_with_compact_block() {
        let config: DaemonConfig =
            toml::from_str("[index.compact_block]\nenabled = false\n").expect("parses");
        let pair = config.compact_block().expect("nothing to check");
        assert_eq!(pair, None, "compact_block() with compact_block off");
        assert_eq!(config.enabled(IndexKind::UtxoSet), None, "utxo_set off with it");
    }

    /// Each row = one rule over an otherwise valid default config (`None` = accepted)
    #[test]
    fn validate_refuses_every_config_no_pipeline_composes_from() {
        type Edit = fn(&mut DaemonConfig);
        fn validators(count: u16) -> Vec<TrustedValidatorConfig> {
            let at = |port| format!("10.0.0.1:{port}");
            let validator = |port| TrustedValidatorConfig {
                jsonrpc_address: at(port),
                ..TrustedValidatorConfig::default()
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
                |c| c.trusted_validators.push(TrustedValidatorConfig::default()),
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
    ///   under `<cache dir>/zaino/indexes/<name>`), written under a directory it creates
    /// - Shipped example parses, its values = the defaults (beside its network, validator, paths)
    #[test]
    fn generated_config_prints_every_key_at_its_default_and_the_example_shows_them() {
        let generated = generate_default_config().expect("generate");
        let body = generated.strip_prefix(GENERATED_CONFIG_HEADER).expect("header first");
        let defaults = DaemonConfig::default();
        let parsed = toml::from_str::<DaemonConfig>(body).expect("parses back");
        assert_eq!(parsed, defaults, "generated config parsed back = DaemonConfig::default()");
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
        for kind in [IndexKind::CompactBlock, IndexKind::UtxoSet, IndexKind::TreeState] {
            let path = defaults.enabled(kind).map(|index| index.path);
            assert_eq!(path, Some(paths::default_index(kind)), "{kind:?}");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new/zainod.toml");
        assert_eq!(generate_config(&path).expect("writes"), ExitCode::SUCCESS, "into new/");
        let written = std::fs::read_to_string(&path).expect("written");
        assert_eq!(written, generated, "{} = the generated config", path.display());

        let example = include_str!("../../../../docs/example_configs/zainod.toml");
        let example = toml::from_str::<DaemonConfig>(example).expect("example parses");
        let validated = example.validate().map_err(|error| error.to_string());
        assert_eq!(validated, Ok(()), "shipped example validates");
        let shown = DaemonConfig {
            network: defaults.network,
            trusted_validators: defaults.trusted_validators.clone(),
            index: defaults.index.clone(),
            ..example
        };
        assert_eq!(shown, defaults, "example's values drifted from DaemonConfig::default()");
    }

    /// Every table but the indexes and validators (their own tests) → the struct its consumer
    /// runs: gRPC server, hold, TLS, validator client, LSM engine, submission, peer set
    ///
    /// - a named key keeps the consumer's own default for every other (`..Default::default()`)
    #[test]
    fn one_toml_reaches_every_consumer_as_written() {
        let config: DaemonConfig = toml::from_str(
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
        let peers = zaino_peers::PeerConfig {
            peer_target: NonZeroUsize::new(12).expect("non-zero"),
            initial_peers: vec!["203.0.113.7:18233".to_owned()],
            cache_dir: Some(PathBuf::from("/var/cache/zaino/peers")),
            ..zaino_peers::PeerConfig::new(NetworkType::Test)
        };
        assert_eq!(config.p2p.peers(config.network), peers, "[p2p] on the declared network");
    }

    /// A value no consumer can run is refused at parse, the message naming key and rule
    #[test]
    fn a_value_no_consumer_can_run_is_refused_at_parse_naming_it() {
        #[rustfmt::skip]
        let refusals = [
            // case,                           toml,                                              refusal names
            ("point reads 0 serve nothing",    "[grpc]\nmax_point_reads = 0",                     ["max_point_reads = 0", "nonzero"]),
            ("range reads 0 serve nothing",    "[grpc]\nmax_range_reads = 0",                     ["max_range_reads = 0", "nonzero"]),
            ("scan reads 0 serve nothing",     "[grpc]\nmax_scan_reads = 0",                      ["max_scan_reads = 0", "nonzero"]),
            ("unread timeout 0",               "[grpc]\nunread_timeout_secs = 0",                 ["unread_timeout_secs = 0", "nonzero"]),
            ("silent timeout 0",               "[grpc]\nsilent_timeout_secs = 0",                 ["silent_timeout_secs = 0", "nonzero"]),
            ("fanout below a merge's 2",       "[sync.lsm_store]\nfanout = 1",                    ["fanout = 1", "a merge needs 2 segments"]),
            ("no merge slot",                  "[sync.lsm_store]\nmerge_slots = 0",               ["merge_slots = 0", "nonzero"]),
            ("merges never paced",             "[sync.lsm_store]\nmerge_mib_per_sec = 0",         ["merge_mib_per_sec = 0", "nonzero"]),
            ("unknown validator client",       "validator_client = \"rpc\"",                      ["validator_client = \"rpc\"", "unknown variant `rpc`"]),
            ("reply cap under one block",      "[[trusted_validators]]\nmax_response_mib = 3",    ["max_response_mib = 3", "must be 4 ..= 32"]),
            ("reply cap past 32 MiB",          "[[trusted_validators]]\nmax_response_mib = 33",   ["max_response_mib = 33", "must be 4 ..= 32"]),
            ("pool under one per lane",        "[[trusted_validators]]\nmax_connections = 4",     ["max_connections = 4", "is below 5"]),
            ("read timeout 0",                 "[[trusted_validators]]\nread_timeout_secs = 0",   ["read_timeout_secs = 0", "nonzero"]),
        ];
        for (case, toml, named) in refusals {
            let refused = toml::from_str::<DaemonConfig>(toml).map_err(|error| error.to_string());
            for name in named {
                assert!(
                    refused.as_ref().is_err_and(|refused| refused.contains(name)),
                    "{case}: refused naming {name:?}, got {refused:#?}"
                );
            }
        }
    }

    /// Entries parsed as written (auth + limits + indexer mutual TLS optional), each → the
    /// balancer's limits, the transport's link, its auth
    #[test]
    fn trusted_validators_parse_and_convert_into_limits_link_and_auth() {
        let config: DaemonConfig = toml::from_str(
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
        let local = TrustedValidatorConfig {
            jsonrpc_address: "127.0.0.1:18232".to_owned(),
            ..TrustedValidatorConfig::default()
        };
        let eu = TrustedValidatorConfig {
            jsonrpc_address: "zebra-eu:8232".to_owned(),
            user: Some("zaino".to_owned()),
            password: Some("secret".to_owned()),
            max_connections: Limits::new(8).expect("8 ≥ MIN_CONNECTIONS"),
            max_response_mib: ResponseMib::try_from(8).expect("in bounds"),
            indexer_address: Some("zebra-eu:8230".to_owned()),
            indexer_tls: Some(ZebraIndexerTls {
                ca_path: "/etc/zaino/indexer-ca.pem".into(),
                cert_path: "/etc/zaino/indexer-client.pem".into(),
                key_path: "/etc/zaino/indexer-client-key.pem".into(),
                server_name: None,
            }),
            priority: 1,
            ..TrustedValidatorConfig::default()
        };
        assert_eq!(config.trusted_validators, [local.clone(), eu.clone()], "both entries");

        let link = LinkLimits {
            max_connections: n(8),
            response_size_cap: ResponseSizeCap::new(8 << 20).expect("in bounds"),
        };
        assert_eq!(eu.link(), link, "the transport's");
        let timeouts = Timeouts { connect: Duration::from_secs(2), read: Duration::from_secs(30) };
        assert_eq!(eu.timeouts(), timeouts, "defaults");

        let basic = Auth::Basic { user: "zaino".to_owned(), password: "secret".to_owned() };
        assert_eq!(eu.auth().expect("a pair"), basic, "user + password");
        assert_eq!(local.auth().expect("neither"), Auth::None, "no credentials");
        let cookie = |user: Option<&str>, password: Option<&str>| TrustedValidatorConfig {
            cookie_path: Some("/zebra/.cookie".into()),
            user: user.map(str::to_owned),
            password: password.map(str::to_owned),
            ..TrustedValidatorConfig::default()
        };
        let cookie_auth = cookie(None, None).auth().expect("a cookie");
        assert_eq!(cookie_auth, Auth::Cookie("/zebra/.cookie".into()), "cookie_path alone");
        let half = TrustedValidatorConfig { password: None, ..eu.clone() };
        let mixes = [
            ("cookie + pair", cookie(Some("u"), Some("p")), "sets cookie_path and user/password"),
            ("half pair", half, "zebra-eu:8232: user and password go together"),
        ];
        for (case, config, named) in mixes {
            let refused = config.auth().map_err(|error| error.to_string());
            assert!(
                refused.as_ref().is_err_and(|refused| refused.contains(named)),
                "{case}: refused naming {named:?}, got {refused:?}"
            );
        }
    }
}
