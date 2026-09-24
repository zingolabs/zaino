//! Cluster sync benchmark — one binary, three modes.
//!
//! Measures how fast the greenfield stack turns a Zebra validator into a built
//! index, over a **bounded** height window so the number is a fixed measurement
//! rather than a sync-to-tip-and-follow. `--mode` selects what is timed:
//!
//! - `provision` — the provisioner alone (source read + compact projection,
//!   output drained). The read-path ceiling.
//! - `sync` — the full pipeline (provisioner → engine → LMDB). The
//!   write-backpressured end-to-end rate. `--verify` then reads a sample back
//!   through the store's compose-on-read path.
//! - `both` — provision then sync over the *same* window in one warm run, then
//!   attribute the bottleneck (read-bound vs engine/write-bound).
//!
//! `--adapter` swaps the source between the on-disk ReadState DB (run on the
//! validator's node) and the validator's JSON-RPC endpoint (`--rpc-addr`); the
//! measured loop is identical over either — it runs over the [`BenchSource`] port.
//!
//! The binary is a thin dispatch: it builds the selected source, resolves the
//! window, and calls the run bodies in [`sync_bench`].
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;

use zaino_consensus::MAX_BLOCK_REORG_HEIGHT;
use zaino_indexer::FetchConcurrency;
use zaino_primitives::types::Height;

use sync_bench::{
    current_tip, default_concurrency, init_logging, open_backend, open_rpc_source, open_source,
    report, report_bottleneck, resume_from_watermark, run_provision, run_sync, verify, window_end,
    AdapterArg, BenchSource, BoxError, Mode, NetworkArg, Outcome,
};

/// Benchmark the greenfield indexing stack over a bounded window.
#[derive(Debug, Parser)]
#[command(name = "sync-bench", about, long_about = None)]
struct Args {
    /// What to measure: `provision` (read-path ceiling), `sync` (full pipeline),
    /// or `both` (run both over one window and attribute the bottleneck).
    #[arg(long, value_enum, default_value_t = Mode::Sync)]
    mode: Mode,

    /// Source adapter. `readstate` opens the on-disk state DB (run on the
    /// validator's node); `rpc` reaches the JSON-RPC endpoint.
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

    /// LMDB directory to build the indexes into (created if absent). Required for
    /// `--mode sync` and `--mode both`; unused by `provision`. Reuse it across
    /// `sync` runs to resume; delete it for a cold build.
    #[arg(long, env = "ZAINO_DB_PATH")]
    db: Option<PathBuf>,

    /// First height of the window. Omitted: genesis, except `--mode sync` which
    /// resumes just past the backend's committed watermark. Set it to bench a
    /// specific chain region — the cumulative indexes then count from this height,
    /// so served tree sizes are window-relative (throughput is unaffected).
    #[arg(long)]
    start: Option<u32>,

    /// Number of blocks in the window. Omitted: to the finalised boundary
    /// (`tip − finalised-depth`) — the whole chain.
    #[arg(long, env = "SYNC_BLOCKS")]
    blocks: Option<u32>,

    /// Engine batch size (blocks committed per atomic batch). `sync`/`both` only.
    #[arg(long, env = "SYNC_BATCH", default_value_t = 1000)]
    batch: u32,

    /// Bound on contexts buffered between the provisioner and its consumer.
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
    /// below is covered. Default is the reorg limit.
    #[arg(long, default_value_t = MAX_BLOCK_REORG_HEIGHT)]
    finalised_depth: u32,

    /// After a `sync`/`both` build, read a sample of blocks back through the
    /// store's compose-on-read path and report their served cumulative tree sizes.
    #[arg(long)]
    verify: bool,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    init_logging();
    let args = Args::parse();

    // Branch on the adapter to build the concrete source, then run the identical
    // generic dispatch over it. This is the only place a concrete adapter appears;
    // everything downstream is over the `BenchSource` port.
    match args.adapter {
        AdapterArg::Readstate => {
            let cache = args
                .zebra_cache
                .as_deref()
                .ok_or("--adapter readstate requires --zebra-cache (or ZEBRA_STATE_DIR)")?;
            let source = open_source(cache, &args.network.to_zebra())?;
            dispatch(source, &args).await
        }
        AdapterArg::Rpc => {
            let source = open_rpc_source(&args.rpc_addr)?;
            dispatch(source, &args).await
        }
    }
}

/// Resolve the window and run the selected mode over `source`.
async fn dispatch<S: BenchSource>(source: Arc<S>, args: &Args) -> Result<(), BoxError> {
    let tip = current_tip(&source).await?;

    match args.mode {
        Mode::Provision => {
            let resume = start_or_genesis(args.start)?;
            let to = window_end(tip, resume, args.blocks, args.finalised_depth)?;
            if is_empty(resume, to, tip, args.finalised_depth) {
                return Ok(());
            }
            announce("provisioning", resume, to, tip, args.adapter, None);
            let out = run_provision(source, resume, to, args.concurrency, args.channel_cap).await?;
            report(
                "provisioned",
                Mode::Provision,
                args.adapter,
                &out,
                args.concurrency,
            );
        }
        Mode::Sync => {
            let db = require_db(args, "sync")?;
            let backend = open_backend(db, args.map_size_gb)?;
            let resume = match args.start {
                Some(height) => Height::try_from(height)?,
                None => resume_from_watermark(&backend)?,
            };
            let to = window_end(tip, resume, args.blocks, args.finalised_depth)?;
            if is_empty(resume, to, tip, args.finalised_depth) {
                return Ok(());
            }
            announce("indexing", resume, to, tip, args.adapter, Some(db));
            let out = run_sync(
                source,
                backend.clone(),
                resume,
                to,
                args.concurrency,
                args.channel_cap,
                args.batch,
            )
            .await?;
            report("indexed", Mode::Sync, args.adapter, &out, args.concurrency);
            if args.verify {
                verify(&backend, resume, to).await?;
            }
        }
        Mode::Both => {
            let db = require_db(args, "both")?;
            let backend = open_backend(db, args.map_size_gb)?;
            // Fixed window (start or genesis) so provision and sync measure the
            // exact same [resume, to] — the comparison must be apples-to-apples.
            let resume = start_or_genesis(args.start)?;
            let to = window_end(tip, resume, args.blocks, args.finalised_depth)?;
            if is_empty(resume, to, tip, args.finalised_depth) {
                return Ok(());
            }
            announce(
                "provisioning+indexing",
                resume,
                to,
                tip,
                args.adapter,
                Some(db),
            );

            let provision: Outcome = run_provision(
                Arc::clone(&source),
                resume,
                to,
                args.concurrency,
                args.channel_cap,
            )
            .await?;
            report(
                "provisioned",
                Mode::Both,
                args.adapter,
                &provision,
                args.concurrency,
            );

            let sync: Outcome = run_sync(
                source,
                backend.clone(),
                resume,
                to,
                args.concurrency,
                args.channel_cap,
                args.batch,
            )
            .await?;
            report("indexed", Mode::Both, args.adapter, &sync, args.concurrency);

            report_bottleneck(args.adapter, &provision, &sync, args.concurrency);
            if args.verify {
                verify(&backend, resume, to).await?;
            }
        }
    }

    Ok(())
}

/// The explicit start height, or genesis when none is given.
fn start_or_genesis(start: Option<u32>) -> Result<Height, BoxError> {
    match start {
        Some(height) => Ok(Height::try_from(height)?),
        None => Ok(Height::GENESIS),
    }
}

/// The LMDB path a mode that builds an index requires.
fn require_db<'a>(args: &'a Args, mode: &str) -> Result<&'a Path, BoxError> {
    args.db
        .as_deref()
        .ok_or_else(|| format!("--mode {mode} requires --db (or ZAINO_DB_PATH)").into())
}

/// Print the "nothing to do" line and report whether the window is empty (resume
/// above the finalised boundary).
fn is_empty(resume: Height, to: Height, tip: Height, finalised_depth: u32) -> bool {
    if resume > to {
        println!(
            "nothing to do: resume height {} is above the finalised boundary {} (tip {} − depth {})",
            u32::from(resume),
            u32::from(to),
            u32::from(tip),
            finalised_depth,
        );
        true
    } else {
        false
    }
}

/// Announce the window a run is about to cover.
fn announce(
    verb: &str,
    resume: Height,
    to: Height,
    tip: Height,
    adapter: AdapterArg,
    db: Option<&Path>,
) {
    let count = u32::from(to) - u32::from(resume) + 1;
    let target = match db {
        Some(path) => format!(" into {}", path.display()),
        None => String::new(),
    };
    println!(
        "{verb} [{}, {}] ({count} blocks) from {} tip {}{target}",
        u32::from(resume),
        u32::from(to),
        adapter.as_str(),
        u32::from(tip),
    );
}
