//! Provisioner-only benchmark.
//!
//! Provisions a chain window from a Zebra validator and times it, **without**
//! indexing: it runs the real [`SourceProvisioner`] and drains its output, so the
//! source read and the compact projection happen but nothing is written to a
//! backend. That isolates the provisioning cost (the read path this measures) from
//! the engine's write path — the natural place to see how the compact read
//! performs on its own.
//!
//! `--adapter readstate` (the default) opens the on-disk state DB and must run on
//! the validator's own node; `--adapter rpc` reaches the validator over its
//! JSON-RPC endpoint (`--rpc-addr`). The same full-chain loop runs over either, so
//! the two numbers isolate the RocksDB-secondary read cost from the RPC-wire cost.
//! By default it provisions the whole chain (genesis → finalised boundary) at the
//! sync engine's default provisioner concurrency.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use tokio::sync::mpsc;

use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;
use zaino_indexer::{CompactBlocks, FetchConcurrency, SourceProvisioner};
use zaino_indexes::sets::current_zaino::context_from_pre_index_compact_block;
use zaino_primitives::types::Height;
use zaino_source::{GetChainTip, GetPreIndexCompactBlock, SubscribeChainTip};

use sync_bench::{
    default_concurrency, init_logging, open_rpc_source, open_source, report, AdapterArg, BoxError,
    NetworkArg,
};

/// Provision a chain window without indexing and report throughput.
#[derive(Debug, Parser)]
#[command(name = "provision-bench", about, long_about = None)]
struct Args {
    /// Source adapter to provision through. `readstate` opens the on-disk state
    /// DB (run on the validator's node); `rpc` reaches the JSON-RPC endpoint.
    #[arg(long, value_enum, default_value_t = AdapterArg::Readstate)]
    adapter: AdapterArg,

    /// Zebra cache directory (the state DB lives under it, per network). Required
    /// for `--adapter readstate`; ignored for `rpc`. Env fallback matches the
    /// value the cluster Job mounts the state at.
    #[arg(long, env = "ZEBRA_STATE_DIR")]
    zebra_cache: Option<PathBuf>,

    /// Validator JSON-RPC endpoint (`host:port`). Used by `--adapter rpc`.
    #[arg(long, env = "ZEBRA_RPC_ADDR", default_value = "127.0.0.1:8232")]
    rpc_addr: String,

    /// Network the validator serves.
    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    network: NetworkArg,

    /// First height to provision. Omitted: genesis.
    #[arg(long, env = "SYNC_START")]
    start: Option<u32>,

    /// Number of blocks to provision. Omitted: to the finalised boundary
    /// (`tip − finalised-depth`) — the whole chain.
    #[arg(long, env = "SYNC_BLOCKS")]
    blocks: Option<u32>,

    /// Fetches kept in flight by the provisioner. Defaults to the sync engine's
    /// default; `0` is rejected at parse time.
    #[arg(long, env = "SYNC_CONCURRENCY", default_value_t = default_concurrency())]
    concurrency: FetchConcurrency,

    /// Bound on contexts buffered out of the provisioner before the drain.
    #[arg(long, default_value_t = 256)]
    channel_cap: usize,

    /// Depth below the tip treated as still volatile; only `tip − depth` and
    /// below is provisioned.
    #[arg(long, default_value_t = MAX_BLOCK_REORG_HEIGHT)]
    finalised_depth: u32,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    init_logging();

    let args = Args::parse();

    // Build whichever source the adapter selects, then run the identical
    // provisioning loop over it. `label` names the source in the report so a run's
    // origin is unambiguous when the two are compared.
    match args.adapter {
        AdapterArg::Readstate => {
            let cache = args
                .zebra_cache
                .as_deref()
                .ok_or("--adapter readstate requires --zebra-cache (or ZEBRA_STATE_DIR)")?;
            let source = open_source(cache, &args.network.to_zebra())?;
            run_provision(source, "ReadState", &args).await
        }
        AdapterArg::Rpc => {
            let source = open_rpc_source(&args.rpc_addr)?;
            run_provision(source, "RPC", &args).await
        }
    }
}

/// Provision `[resume, to]` through `source` and report the throughput. Generic
/// over the source so the ReadState and RPC adapters run the exact same loop —
/// the only difference between the two runs is the transport `source` speaks.
async fn run_provision<S>(source: Arc<S>, label: &str, args: &Args) -> Result<(), BoxError>
where
    S: GetChainTip + SubscribeChainTip + GetPreIndexCompactBlock + Send + Sync + 'static,
{
    // The same provisioner the indexer runs, over the compact-block fetch
    // strategy, projecting each fetched pre-index compact block into the set-wide
    // context — exactly the read work indexing does, and nothing else.
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, CompactBlocks>::new(
        source,
        |cb| context_from_pre_index_compact_block(&cb),
        args.concurrency,
    ));

    let resume = match args.start {
        Some(height) => Height::try_from(height)?,
        None => Height::GENESIS,
    };
    let tip = provisioner.current_tip().await?;
    let finalised = tip.saturating_sub(args.finalised_depth);
    let to = match args.blocks {
        Some(count) if count > 0 => resume
            .checked_add(count - 1)
            .ok_or("requested window exceeds the protocol limit")?
            .min(finalised),
        _ => finalised,
    };

    if resume > to {
        println!(
            "nothing to provision: resume height {} is above the finalised boundary {}",
            u32::from(resume),
            u32::from(finalised),
        );
        return Ok(());
    }

    let count = u32::from(to) - u32::from(resume) + 1;
    println!(
        "provisioning [{}, {}] ({count} blocks) from {label} tip {}",
        u32::from(resume),
        u32::from(to),
        u32::from(tip),
    );

    // Provision into the channel and drain it, dropping each context. The drain
    // is the whole point: it forces the provisioner to actually fetch and project
    // every block, with no engine on the other end to attribute cost to.
    let (tx, mut rx) = mpsc::channel(args.channel_cap);
    let feed = Arc::clone(&provisioner);
    let provision = tokio::spawn(async move { feed.provision(resume, to, tx).await });

    let started = Instant::now();
    while rx.recv().await.is_some() {}
    provision.await??;
    let elapsed = started.elapsed();

    report("provisioned", count, elapsed, args.concurrency);

    Ok(())
}
