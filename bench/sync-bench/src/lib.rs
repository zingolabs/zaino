//! Shared engine for the `sync-bench` harness binary.
//!
//! One binary, three modes (see the `sync-bench` bin): `provision` drains the
//! provisioner (source read + compact projection, no engine), `sync` runs the
//! full provisioner → engine → LMDB pipeline, and `both` runs the two over the
//! same window in one warm environment and attributes the bottleneck.
//!
//! This module owns the two run bodies (`run_provision`, `run_sync`), the window
//! resolution, and the result reporting; the binary is a thin dispatch on the
//! selected mode. Both run bodies drive the *same* generic [`BenchSource`] port,
//! so the ReadState and RPC adapters exercise byte-for-byte the same paths.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_core::BlockRef;
use zaino_indexer::{CompactBlocks, FetchConcurrency, SourceProvisioner};
use zaino_indexes::sets::current_zaino::{
    context_from_pre_index_compact_block, index_set, CurrentZaino, CurrentZainoContext,
};
use zaino_persistence::Namespace;
use zaino_persistence_codec::reserved_namespaces;
use zaino_primitives::types::Height;
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_service::{CompactBlockRead, TakeSnapshot};
use zaino_source::{
    GetChainTip, GetPreIndexCompactBlock, RetryPolicy, SubscribeChainTip, ValidatorClient,
};
use zaino_source_zebra::ZebraValidator;
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zaino_source_zebra_rpc::ZebraRpcAdapter;
use zaino_store::StoreReader;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;
use zebra_chain::parameters::Network;

/// A boxed error is enough for a benchmark binary — every step already carries a
/// typed cause, and the harness only reports the failure, it does not react to
/// its variant.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The source port the benchmarks provision through. Both run bodies are generic
/// over this one bound, so the concrete adapter is swapped at the composition
/// root (`--adapter`) while the measured loop stays identical — the whole point
/// of benching over the port rather than a concrete source. The bound is the
/// exact set [`SourceProvisioner`] requires: read the tip, follow it, and fetch
/// pre-index compact blocks.
pub trait BenchSource:
    GetChainTip + SubscribeChainTip + GetPreIndexCompactBlock + Send + Sync + 'static
{
}

impl<T> BenchSource for T where
    T: GetChainTip + SubscribeChainTip + GetPreIndexCompactBlock + Send + Sync + 'static
{
}

/// The resilient ReadState source: reads compact blocks straight off the
/// on-disk state DB. The default adapter for both modes.
pub type Source = Arc<ValidatorClient<ZebraReadStateAdapter>>;

/// The resilient JSON-RPC source: reads compact blocks over the validator's RPC
/// endpoint, to isolate the RocksDB-secondary read cost from the RPC-wire cost on
/// the same loop.
///
/// It is the `ZebraValidator` composite (`rpc_only`), exactly as production RPC
/// mode composes it — the bare RPC adapter has no tip stream, so the composite
/// supplies one by polling. Benching the composite therefore measures the real
/// RPC source path, not a stripped-down one.
pub type RpcSource = Arc<ValidatorClient<ZebraValidator>>;

/// What the benchmark measures: `provision` drains the provisioner (read-path
/// ceiling), `sync` runs the full pipeline (write-backpressured end-to-end), and
/// `both` runs the two over one window and attributes the bottleneck.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Mode {
    /// Provisioner only: source read + compact projection, output drained.
    Provision,
    /// Full pipeline: provisioner → engine → LMDB.
    Sync,
    /// Provision then sync over the same window; report which side bounds it.
    Both,
}

impl Mode {
    /// A stable lowercase tag for the result schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Provision => "provision",
            Mode::Sync => "sync",
            Mode::Both => "both",
        }
    }
}

/// Which source adapter a run provisions through — the two ports the same loop
/// can run over.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AdapterArg {
    /// Open the on-disk Zebra state DB directly (run on the validator's node).
    Readstate,
    /// Reach the validator over its JSON-RPC endpoint.
    Rpc,
}

impl AdapterArg {
    /// A stable lowercase tag for the result schema.
    pub fn as_str(self) -> &'static str {
        match self {
            AdapterArg::Readstate => "readstate",
            AdapterArg::Rpc => "rpc",
        }
    }
}

/// The sync engine's default provisioner concurrency — the value a real indexer
/// runs with, so a bench that does not sweep the knob reflects it rather than a
/// swept value.
pub fn default_concurrency() -> FetchConcurrency {
    FetchConcurrency::new(NonZeroUsize::new(16).expect("16 is non-zero"))
}

/// The networks the harness supports, mapped to zebra's [`Network`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum NetworkArg {
    /// The Zcash main network.
    Mainnet,
    /// The Zcash test network.
    Testnet,
}

impl NetworkArg {
    /// The zebra [`Network`] this argument selects.
    pub fn to_zebra(self) -> Network {
        match self {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::new_default_testnet(),
        }
    }
}

/// Install the JSON tracing subscriber (→ stdout → Loki/Grafana on the cluster).
pub fn init_logging() {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// Open the Zebra ReadState under `cache` (read-only) wrapped in the resilient
/// decorator, so a run binds the resilient source ports rather than the raw
/// single-attempt adapter.
pub fn open_source(cache: &Path, network: &Network) -> Result<Source, BoxError> {
    let adapter = ZebraReadStateAdapter::open(cache, network)?;
    Ok(Arc::new(ValidatorClient::new(
        adapter,
        RetryPolicy::default(),
    )))
}

/// Connect to the validator's JSON-RPC endpoint at `addr` (`host:port`), wrapped
/// in the same resilient decorator as [`open_source`], so the two adapters differ
/// only in transport and the loop over them is identical.
pub fn open_rpc_source(addr: &str) -> Result<RpcSource, BoxError> {
    let rpc = RpcClient::new(RpcClientConfig {
        url: format!("http://{addr}"),
        ..RpcClientConfig::default()
    })?;
    let validator = ZebraValidator::rpc_only(ZebraRpcAdapter::new(rpc));
    Ok(Arc::new(ValidatorClient::new(
        validator,
        RetryPolicy::default(),
    )))
}

/// Open the LMDB backend the engine builds into, declaring every namespace up
/// front — one per index in the set, plus the engine's reserved watermark /
/// format-version namespaces.
pub fn open_backend(db: &Path, map_size_gb: usize) -> Result<LmdbBackend, BoxError> {
    let namespaces: Vec<Namespace> = index_set()
        .index_ids()
        .into_iter()
        .map(Namespace::from)
        .chain(reserved_namespaces())
        .collect();
    Ok(LmdbBackend::open(LmdbConfig {
        path: db.to_path_buf(),
        map_size_bytes: map_size_gb << 30,
        namespaces,
    })?)
}

/// The height a `sync` run resumes at when no explicit start is given: just past
/// the backend's committed watermark, or genesis on a fresh backend.
pub fn resume_from_watermark(backend: &LmdbBackend) -> Result<Height, BoxError> {
    match SyncEngine::<CurrentZainoContext, _>::committed_height(backend)? {
        Some(committed) => Ok(Height::try_from(u32::try_from(committed.value())?)?
            .checked_add(1)
            .ok_or("committed watermark is already at the protocol limit")?),
        None => Ok(Height::GENESIS),
    }
}

/// The validator's current tip, via the same mapping the provisioner uses.
pub async fn current_tip<S: BenchSource>(source: &Arc<S>) -> Result<Height, BoxError> {
    let provisioner = SourceProvisioner::<_, _, _, CompactBlocks>::new(
        Arc::clone(source),
        |cb| context_from_pre_index_compact_block(&cb),
        default_concurrency(),
    );
    Ok(provisioner.current_tip().await?)
}

/// The last height a run covers: the requested count from `resume`, capped at the
/// finalised boundary (`tip − finalised_depth`), or the boundary itself.
pub fn window_end(
    tip: Height,
    resume: Height,
    blocks: Option<u32>,
    finalised_depth: u32,
) -> Result<Height, BoxError> {
    let finalised = tip.saturating_sub(finalised_depth);
    Ok(match blocks {
        Some(count) if count > 0 => resume
            .checked_add(count - 1)
            .ok_or("requested window exceeds the protocol limit")?
            .min(finalised),
        _ => finalised,
    })
}

/// A completed run: how many blocks over how long.
pub struct Outcome {
    /// Blocks covered by the window.
    pub count: u32,
    /// Wall-clock time the measured section took.
    pub elapsed: Duration,
}

impl Outcome {
    /// Throughput in blocks per second.
    pub fn blocks_per_second(&self) -> f64 {
        f64::from(self.count) / self.elapsed.as_secs_f64()
    }
}

/// Provision `[resume, to]` and drain the output, dropping each context. The
/// drain forces the provisioner to fetch and project every block with no engine
/// to attribute cost to — the read-path ceiling.
pub async fn run_provision<S: BenchSource>(
    source: Arc<S>,
    resume: Height,
    to: Height,
    concurrency: FetchConcurrency,
    channel_cap: usize,
) -> Result<Outcome, BoxError> {
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, CompactBlocks>::new(
        source,
        |cb| context_from_pre_index_compact_block(&cb),
        concurrency,
    ));
    let count = u32::from(to) - u32::from(resume) + 1;

    let (tx, mut rx) = mpsc::channel(channel_cap);
    let feed = Arc::clone(&provisioner);
    let provision = tokio::spawn(async move { feed.provision(resume, to, tx).await });

    let started = Instant::now();
    while rx.recv().await.is_some() {}
    provision.await??;
    Ok(Outcome {
        count,
        elapsed: started.elapsed(),
    })
}

/// Run the full pipeline over `[resume, to]`: provision → engine → LMDB. This is
/// the driver's `sync_to` body minus the tip-follow loop, so the throughput is
/// the write-backpressured end-to-end rate.
pub async fn run_sync<S: BenchSource>(
    source: Arc<S>,
    backend: LmdbBackend,
    resume: Height,
    to: Height,
    concurrency: FetchConcurrency,
    channel_cap: usize,
    batch: u32,
) -> Result<Outcome, BoxError> {
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, CompactBlocks>::new(
        source,
        |cb| context_from_pre_index_compact_block(&cb),
        concurrency,
    ));
    let count = u32::from(to) - u32::from(resume) + 1;

    let mut engine = SyncEngine::from_index_set(
        index_set(),
        backend,
        EngineConfig {
            batch_size: batch,
            start_height: BlockHeight::new(u64::from(resume)),
        },
    )?;

    let (tx, rx) = mpsc::channel(channel_cap);
    let feed = Arc::clone(&provisioner);
    let provision = tokio::spawn(async move { feed.provision(resume, to, tx).await });

    let started = Instant::now();
    engine.sync_channel(rx).await?;
    provision.await??;
    Ok(Outcome {
        count,
        elapsed: started.elapsed(),
    })
}

/// Report one run as a human line and a structured event tagged with the full
/// `(kind, mode, adapter)` tuple, so cross-commit runs are comparable in
/// Loki/Grafana. `kind` names the work timed ("provisioned" or "indexed").
pub fn report(
    kind: &str,
    mode: Mode,
    adapter: AdapterArg,
    out: &Outcome,
    concurrency: FetchConcurrency,
) {
    let seconds = out.elapsed.as_secs_f64();
    let per_second = out.blocks_per_second();
    println!(
        "{kind} [{}/{}] {} blocks in {seconds:.3}s = {per_second:.1} blocks/s \
         ({:.3} ms/block, concurrency {concurrency})",
        mode.as_str(),
        adapter.as_str(),
        out.count,
        seconds * 1000.0 / f64::from(out.count),
    );
    tracing::info!(
        target: "sync_bench::result",
        kind,
        mode = mode.as_str(),
        adapter = adapter.as_str(),
        blocks = out.count,
        elapsed_ms = out.elapsed.as_millis(),
        blocks_per_second = per_second,
        concurrency = concurrency.get(),
        "bench window complete"
    );
}

/// Attribute the bottleneck from a `both` run's two outcomes.
///
/// This is a **comparison of two regimes** — the free-running read-path ceiling
/// (`provision`) against the write-backpressured end-to-end rate (`sync`) — not a
/// strict time subtraction: in `sync` the engine's write rate backpressures the
/// provisioner, so provisioning never runs at its ceiling. When end-to-end is
/// close to the ceiling the read path bounds throughput; when it is far below,
/// the engine (index build + LMDB write) does, and `ceiling − end-to-end` is the
/// headroom recoverable by optimising the write side.
pub fn report_bottleneck(
    adapter: AdapterArg,
    provision: &Outcome,
    sync: &Outcome,
    concurrency: FetchConcurrency,
) {
    let ceiling = provision.blocks_per_second();
    let end_to_end = sync.blocks_per_second();
    let verdict = attribute(ceiling, end_to_end);

    println!(
        "bottleneck [{}] read-ceiling {ceiling:.1} b/s vs end-to-end {end_to_end:.1} b/s \
         (ratio {:.2}) -> {}; write headroom {:.1} b/s",
        adapter.as_str(),
        verdict.ratio,
        verdict.bound,
        verdict.headroom,
    );
    tracing::info!(
        target: "sync_bench::result",
        kind = "bottleneck",
        mode = Mode::Both.as_str(),
        adapter = adapter.as_str(),
        provision_bps = ceiling,
        sync_bps = end_to_end,
        ratio = verdict.ratio,
        headroom_bps = verdict.headroom,
        bound = verdict.bound,
        concurrency = concurrency.get(),
        "bottleneck attribution"
    );
}

/// The bottleneck verdict: how close end-to-end runs to the read ceiling, the
/// recoverable write headroom, and which side to optimise.
struct Attribution {
    ratio: f64,
    headroom: f64,
    bound: &'static str,
}

/// Attribute the bottleneck from the read-path ceiling and the write-backpressured
/// end-to-end rate (both blocks/s). End-to-end within [`READ_BOUND_RATIO`] of the
/// ceiling reads as read-bound; otherwise the engine (index build + LMDB write)
/// bounds it and `ceiling − end-to-end` is the recoverable headroom.
fn attribute(ceiling: f64, end_to_end: f64) -> Attribution {
    /// End-to-end within this fraction of the ceiling reads as read-bound.
    const READ_BOUND_RATIO: f64 = 0.85;

    let ratio = if ceiling > 0.0 {
        end_to_end / ceiling
    } else {
        0.0
    };
    let headroom = (ceiling - end_to_end).max(0.0);
    let bound = if ratio >= READ_BOUND_RATIO {
        "provisioning/read-bound (optimize the source read path)"
    } else {
        "engine/processing+io-bound (optimize index build + LMDB write)"
    };
    Attribution {
        ratio,
        headroom,
        bound,
    }
}

/// Read a sample of the just-built window back through the store's
/// compose-on-read path, confirming it serves composed compact blocks whose
/// cumulative tree sizes never decrease across the window.
pub async fn verify(backend: &LmdbBackend, from: Height, to: Height) -> Result<(), BoxError> {
    let reader = StoreReader::<_, CurrentZaino>::new(Arc::new(backend.clone()));
    let snapshot = reader.snapshot().await.map_err(|e| e.to_string())?;

    // Up to `SAMPLES` evenly-spaced heights across the window; the endpoints are
    // always the first and last. `dedup` collapses the repeats a window shorter
    // than `SAMPLES` produces, so a single-block window verifies exactly once.
    const SAMPLES: u32 = 10;
    let span = u32::from(to) - u32::from(from);
    let divisor = (SAMPLES - 1).max(1);
    let mut heights: Vec<u32> = (0..SAMPLES)
        .map(|i| u32::from(from) + (span * i) / divisor)
        .collect();
    heights.dedup();

    println!(
        "verifying composed compact blocks at {} sample heights:",
        heights.len()
    );
    let mut prev: Option<(u32, u32, u32)> = None;
    for height in heights {
        let block = snapshot
            .compact_block(BlockRef::Height(Height::try_from(height)?))
            .await?
            .ok_or_else(|| format!("no composed compact block served at height {height}"))?;
        let sapling = u32::from(block.chain_metadata.sapling_tree_size);
        let orchard = u32::from(block.chain_metadata.orchard_tree_size);
        let txs = block.transactions.len();
        println!(
            "  h={height:>8}  txs={txs:>4}  sapling_tree={sapling:>10}  orchard_tree={orchard:>10}"
        );
        if let Some((prev_h, prev_sapling, prev_orchard)) = prev {
            if sapling < prev_sapling || orchard < prev_orchard {
                return Err(format!(
                    "cumulative tree size decreased between h={prev_h} \
                     (sapling {prev_sapling}, orchard {prev_orchard}) and h={height} \
                     (sapling {sapling}, orchard {orchard})"
                )
                .into());
            }
        }
        prev = Some((height, sapling, orchard));
    }
    println!("verify OK: served compact blocks with non-decreasing cumulative tree sizes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::attribute;

    #[test]
    fn end_to_end_near_ceiling_is_read_bound() {
        let verdict = attribute(1000.0, 900.0);
        assert!(verdict.bound.contains("read-bound"), "{}", verdict.bound);
        assert!((verdict.ratio - 0.9).abs() < 1e-9);
        assert!((verdict.headroom - 100.0).abs() < 1e-9);
    }

    #[test]
    fn end_to_end_far_below_ceiling_is_write_bound() {
        let verdict = attribute(1000.0, 400.0);
        assert!(verdict.bound.contains("io-bound"), "{}", verdict.bound);
        assert!((verdict.ratio - 0.4).abs() < 1e-9);
        assert!((verdict.headroom - 600.0).abs() < 1e-9);
    }

    #[test]
    fn a_zero_ceiling_never_divides_by_zero() {
        let verdict = attribute(0.0, 0.0);
        assert_eq!(verdict.ratio, 0.0);
        assert_eq!(verdict.headroom, 0.0);
        assert!(verdict.bound.contains("io-bound"));
    }
}
