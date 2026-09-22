//! Cluster sync benchmark.
//!
//! Drives the real provisioner → engine pipeline over a **bounded** height
//! window and times it: it sources pre-index compact blocks from a Zebra
//! ReadState validator (the fast path — no RPC, no proof/signature decode),
//! projects them into the current-zaino index set, and builds that set into an
//! LMDB backend. With `--verify` it then reads a sample back through the store's
//! compose-on-read path, confirming the store serves composed compact blocks
//! whose cumulative tree sizes are internally consistent.
//!
//! It deliberately drives the pipeline directly rather than through
//! [`SourceSyncDriver`]'s run loop: that loop syncs to the finalised tip and
//! then follows forever, which cannot express the fixed measurement window a
//! throughput bench needs. The provision → `sync_channel` body it runs here is
//! exactly the driver's [`sync_to`], so the numbers reflect the production path
//! minus only the tip-follow select loop.
//!
//! Run it on the validator's own node: ReadState opens the on-disk state DB, so
//! the source cost is a local read and the measurement isolates indexing from
//! network variance.
//!
//! [`SourceSyncDriver`]: zaino_indexer::SourceSyncDriver
//! [`sync_to`]: zaino_indexer::SourceSyncDriver
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use tokio::sync::mpsc;

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;
use zaino_core::BlockRef;
use zaino_indexer::{CompactBlocks, FetchConcurrency, SourceProvisioner};
use zaino_indexes::sets::current_zaino::{
    context_from_pre_index_compact_block, index_set, CurrentZainoContext,
};
use zaino_persistence::Namespace;
use zaino_persistence_codec::reserved_namespaces;
use zaino_primitives::types::Height;
use zaino_service::{CompactBlockRead, TakeSnapshot};
use zaino_store::StoreReader;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;

use sync_bench::{default_concurrency, init_logging, open_source, report, BoxError, NetworkArg};

/// Index a bounded window of pre-index compact blocks from a Zebra ReadState
/// validator into LMDB and report throughput.
#[derive(Debug, Parser)]
#[command(name = "sync-bench", about, long_about = None)]
struct Args {
    /// Zebra cache directory (the state DB lives under it, per network). Env
    /// fallback matches the value the cluster Job mounts the state at.
    #[arg(long, env = "ZEBRA_STATE_DIR")]
    zebra_cache: PathBuf,

    /// LMDB directory to build the indexes into (created if absent). Reuse it
    /// across runs to resume; delete it for a cold build.
    #[arg(long, env = "ZAINO_DB_PATH")]
    db: PathBuf,

    /// Network the validator serves.
    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    network: NetworkArg,

    /// First height to index this run. Omitted: resume just past the backend's
    /// committed watermark, or genesis on a fresh backend. Set it to bench a
    /// specific chain region on a fresh DB — note the cumulative indexes then
    /// count from this height, so served tree sizes are window-relative, not
    /// absolute (throughput is unaffected; `--verify` checks consistency only).
    #[arg(long)]
    start: Option<u32>,

    /// Number of blocks to index this run. Omitted: index to the finalised
    /// boundary (`tip − finalised-depth`).
    #[arg(long, env = "SYNC_BLOCKS")]
    blocks: Option<u32>,

    /// Engine batch size (blocks committed per atomic batch).
    #[arg(long, env = "SYNC_BATCH", default_value_t = 1000)]
    batch: u32,

    /// Bound on contexts buffered between the provisioner and the engine.
    #[arg(long, default_value_t = 256)]
    channel_cap: usize,

    /// Fetches kept in flight by the provisioner (1 = serial). The knob for the
    /// concurrency-vs-throughput sweep; `0` is rejected at parse time.
    #[arg(long, env = "SYNC_CONCURRENCY", default_value_t = default_concurrency())]
    concurrency: FetchConcurrency,

    /// LMDB map size in GiB (the maximum on-disk size; reserved up front).
    #[arg(long, default_value_t = 16)]
    map_size_gb: usize,

    /// Depth below the tip treated as still volatile; only `tip − depth` and
    /// below is indexed. Standalone default is the reorg limit.
    #[arg(long, default_value_t = MAX_BLOCK_REORG_HEIGHT)]
    finalised_depth: u32,

    /// After indexing, read a sample of blocks back through the store's
    /// compose-on-read path and report their served cumulative tree sizes.
    #[arg(long)]
    verify: bool,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    init_logging();

    let args = Args::parse();
    let network = args.network.to_zebra();

    // Source: the ReadState adapter opens the on-disk state DB read-only, wrapped
    // in the resilient decorator so the provisioner binds the resilient ports.
    // Retry stays at the policy default; on a local read it should never fire.
    let source = open_source(&args.zebra_cache, &network)?;

    // Backend: LMDB must declare every namespace up front — one per index in the
    // set, plus the engine's reserved watermark / format-version namespaces.
    let namespaces: Vec<Namespace> = index_set()
        .index_ids()
        .into_iter()
        .map(Namespace::from)
        .chain(reserved_namespaces())
        .collect();
    let backend = LmdbBackend::open(LmdbConfig {
        path: args.db.clone(),
        map_size_bytes: args.map_size_gb << 30,
        namespaces,
    })?;

    // Where to start: forced region, resume point, or genesis.
    let resume = match args.start {
        Some(height) => Height::try_from(height)?,
        None => match SyncEngine::<CurrentZainoContext, _>::committed_height(&backend)? {
            Some(committed) => Height::try_from(u32::try_from(committed.value())?)?
                .checked_add(1)
                .ok_or("committed watermark is already at the protocol limit")?,
            None => Height::GENESIS,
        },
    };

    // The provisioner over the compact-block fetch strategy, projecting each
    // fetched pre-index compact block into the set-wide context.
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, CompactBlocks>::new(
        Arc::clone(&source),
        |cb| context_from_pre_index_compact_block(&cb),
        args.concurrency,
    ));

    // The window: from the resume point to either the requested count or the
    // finalised boundary, whichever is lower.
    let tip = provisioner.current_tip().await?;
    let finalised = tip.saturating_sub(args.finalised_depth);
    let to = match args.blocks {
        Some(count) if count > 0 => {
            let requested_end = resume
                .checked_add(count - 1)
                .ok_or("requested window exceeds the protocol limit")?;
            requested_end.min(finalised)
        }
        _ => finalised,
    };

    if resume > to {
        println!(
            "nothing to index: resume height {} is above the finalised boundary {} \
             (tip {} − depth {})",
            u32::from(resume),
            u32::from(finalised),
            u32::from(tip),
            args.finalised_depth,
        );
        return Ok(());
    }

    let count = u32::from(to) - u32::from(resume) + 1;
    println!(
        "indexing [{}, {}] ({count} blocks) from ReadState tip {} into {}",
        u32::from(resume),
        u32::from(to),
        u32::from(tip),
        args.db.display(),
    );

    // Build the engine at the resume point, then run provision → sync_channel —
    // the driver's sync_to body — over the bounded window, timing it.
    let mut engine = SyncEngine::from_index_set(
        index_set(),
        backend.clone(),
        EngineConfig {
            batch_size: args.batch,
            start_height: BlockHeight::new(u64::from(resume)),
        },
    )?;

    let (tx, rx) = mpsc::channel(args.channel_cap);
    let feed = Arc::clone(&provisioner);
    let provision = tokio::spawn(async move { feed.provision(resume, to, tx).await });

    let started = Instant::now();
    engine.sync_channel(rx).await?;
    provision.await??;
    let elapsed = started.elapsed();

    report("indexed", count, elapsed, args.concurrency);

    if args.verify {
        verify(&backend, resume, to).await?;
    }

    Ok(())
}

/// Read a sample of the just-built window back through the store's
/// compose-on-read path, confirming it serves composed compact blocks whose
/// cumulative tree sizes never decrease across the window.
async fn verify(backend: &LmdbBackend, from: Height, to: Height) -> Result<(), BoxError> {
    let reader = StoreReader::new(Arc::new(backend.clone()));
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
