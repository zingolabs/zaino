use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use clap::{Parser, Subcommand};
use tracing::info;
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::serialization::ZcashSerialize;
use zebra_state::Config as ZebraConfig;

use zaino_state::chain_index::types::{BlockMetadata, BlockWithMetadata, ChainWork, IndexedBlock};
use zaino_store::lmdb::LmdbStore;
use zaino_store::types::MAX_REORG_DEPTH;
use zaino_store::Block as StoreBlock;

mod block_compare;
mod check;
mod compare;
mod concurrent;
mod grpc_client;
mod grpc_test;

type AdminResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug)]
struct AdminError(String);

impl std::fmt::Display for AdminError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AdminError {}

fn boxed_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(AdminError(message.into()))
}

/// Administration tool for managing a Zaino node.
#[derive(Parser)]
#[command(name = "zaino-admin")]
struct Cli {
    /// Path to the LMDB data directory (the directory containing `data.mdb`
    /// and `lock.mdb`).
    #[arg(short, long, default_value = "data/lmdb")]
    db_path: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate prev_hash links across compact blocks served over gRPC.
    Check(check::CheckArgs),
    /// Compare CompactBlock output from two lightwalletd servers.
    Compare(compare::CompareArgs),
    /// Run a concurrent block-range load test against a lightwalletd server.
    ConcurrentTest(concurrent::ConcurrentTestArgs),
    /// Exercise every CompactTxStreamer gRPC method.
    GrpcTest(grpc_test::GrpcTestArgs),
    /// Trim the block store back to a known-good height.
    ///
    /// Deletes all LMDB entries above the given height and resets the sentinel
    /// so that the sync loop resumes from the trimmed point on the next run.
    /// The in-memory state (PHM + deque) is not touched — on the next startup,
    /// ChainState::open reads the truncated LMDB and restores the last ~100
    /// blocks from there.
    Trim {
        /// Height to truncate to (inclusive). Blocks above this height are
        /// deleted from LMDB.
        height: u32,

        /// Actually apply the truncation. Without this flag, only prints
        /// what would be deleted (dry-run).
        #[arg(short, long)]
        yes: bool,
    },
    /// Bootstrap the block store from a Zebra state database.
    ///
    /// Reads blocks directly from Zebra's RocksDB and writes them to the
    /// zaino-store LMDB. No network needed, no validator required. Uses the
    /// network and start_height from a zainod config file.
    Bootstrap {
        /// Path to Zebra's cache directory (e.g. ~/.cache/zebra).
        zebra_db_dir: PathBuf,

        /// Path to the zainod TOML config file. Reads `network` and
        /// `start_height` from this file. Defaults to
        /// `$XDG_CONFIG_HOME/zaino/zainod.toml`.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Sequential read benchmark — reads and parses blocks with no
    /// tree lookup, no chainwork, no LMDB writes. Isolates raw
    /// RocksDB read + Zcash deserialization throughput.
    Scan {
        /// Path to Zebra's cache directory (e.g. ~/.cache/zebra).
        zebra_db_dir: PathBuf,

        /// Path to the zainod TOML config file.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zaino_admin=info".into()),
        )
        .init();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}

async fn run(cli: Cli) -> AdminResult<()> {
    match cli.command {
        Command::Check(args) => check::run(args).await,
        Command::Compare(args) => compare::run(args).await,
        Command::ConcurrentTest(args) => concurrent::run(args).await,
        Command::GrpcTest(args) => grpc_test::run(args).await,
        Command::Trim { height, yes } => trim(&cli.db_path, height, yes),
        Command::Bootstrap {
            zebra_db_dir,
            config,
        } => {
            let config_path = config.unwrap_or_else(|| {
                zaino_common::xdg::resolve_path_with_xdg_config_defaults("zaino/zainod.toml")
            });
            bootstrap(&cli.db_path, &zebra_db_dir, &config_path)
        }
        Command::Scan {
            zebra_db_dir,
            config,
        } => {
            let config_path = config.unwrap_or_else(|| {
                zaino_common::xdg::resolve_path_with_xdg_config_defaults("zaino/zainod.toml")
            });
            scan(&zebra_db_dir, &config_path)
        }
    }
}

fn trim(db_path: &Path, height: u32, yes: bool) -> AdminResult<()> {
    let store = LmdbStore::open(db_path)?;

    let latest = match store.block_count()? {
        Some(c) if c > 0 => c - 1,
        _ => {
            println!("LMDB is empty — nothing to trim.");
            return Ok(());
        }
    };

    if height >= latest {
        println!(
            "Nothing to trim: target height {} is >= current tip {}.",
            height, latest
        );
        return Ok(());
    }

    let to_delete = latest - height;
    println!("LMDB path:    {}", db_path.display());
    println!("Current tip:  {}", latest);
    println!("Target tip:   {}", height);
    println!(
        "To delete:    {} blocks (heights {}..={})",
        to_delete,
        height + 1,
        latest
    );

    if !yes {
        println!();
        println!("Dry-run complete. Use --yes to apply the truncation.");
        return Ok(());
    }

    let deleted = store.truncate_to_height(height)?;
    println!("Deleted {} blocks.", deleted);

    println!("New tip:      {}", store.block_count()?.map(|c| c - 1).unwrap_or(0));
    println!("Done. Restart zaino to resume sync from height {}.", height);

    Ok(())
}

/// Fields read from the zainod TOML config.
#[derive(serde::Deserialize)]
struct BootstrapConfig {
    network: zaino_common::Network,
    #[serde(default)]
    start_height: Option<u32>,
    #[serde(default)]
    storage: Option<BootstrapStorage>,
}

#[derive(serde::Deserialize)]
struct BootstrapStorage {
    database: Option<BootstrapDatabase>,
}

#[derive(serde::Deserialize)]
struct BootstrapDatabase {
    path: Option<PathBuf>,
}

fn load_bootstrap_config(
    config_path: &Path,
) -> Result<BootstrapConfig, Box<dyn std::error::Error>> {
    let toml_str = fs::read_to_string(config_path)?;
    Ok(toml::from_str(&toml_str)?)
}

fn resolve_block_store_path(config_path: &Path, cfg: &BootstrapConfig) -> PathBuf {
    // Config file's storage.database.path takes priority.
    if let Some(ref storage) = cfg.storage {
        if let Some(ref database) = storage.database {
            if let Some(ref path) = database.path {
                return path.join("block_store");
            }
        }
    }
    // Fall back to the directory of the config file + block_store.
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("block_store")
}

// All column families that Zebra's state RocksDB uses. Must match
// zebra_state::service::finalized_state::STATE_COLUMN_FAMILIES_IN_CODE.
// Kept here because that constant is not publicly exported.
const STATE_COLUMN_FAMILIES: &[&str] = &[
    "hash_by_height",
    "height_by_hash",
    "block_header_by_height",
    "tx_by_loc",
    "hash_by_tx_loc",
    "tx_loc_by_hash",
    "balance_by_transparent_addr",
    "tx_loc_by_transparent_addr_loc",
    "utxo_by_out_loc",
    "utxo_loc_by_transparent_addr_loc",
    "tx_loc_by_spent_out_loc",
    "sprout_nullifiers",
    "sprout_anchors",
    "sprout_note_commitment_tree",
    "sapling_nullifiers",
    "sapling_anchors",
    "sapling_note_commitment_tree",
    "sapling_note_commitment_subtree",
    "orchard_nullifiers",
    "orchard_anchors",
    "orchard_note_commitment_tree",
    "orchard_note_commitment_subtree",
    "history_tree",
    "tip_chain_value_pool",
    "block_info",
];

fn scan(zebra_db_dir: &Path, config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_bootstrap_config(config_path)?;
    let zebra_network = cfg.network.to_zebra_network();

    let default_start = NetworkUpgrade::Sapling
        .activation_height(&zebra_network)
        .expect("Sapling activation height must be set")
        .0;
    let start_height = cfg.start_height.unwrap_or(default_start);

    let zebra_config = ZebraConfig {
        cache_dir: zebra_db_dir.to_path_buf(),
        ..Default::default()
    };
    let zebra_db = zebra_state::ZebraDb::new(
        &zebra_config,
        "state",
        &zebra_state::state_database_format_version_in_code(),
        &zebra_network,
        false,
        STATE_COLUMN_FAMILIES.iter().map(|s| s.to_string()),
        true,
    );

    let (tip_height, _) = zebra_db.tip().ok_or("Zebra state database is empty")?;
    let finalized_end = tip_height.0.saturating_sub(MAX_REORG_DEPTH);

    let start_time = std::time::Instant::now();
    let mut bytes_read: u64 = 0;
    let mut tx_count: u64 = 0;

    for h in start_height..=finalized_end {
        let height = zebra_chain::block::Height(h);
        let block = zebra_db
            .block(zebra_state::HashOrHeight::Height(height))
            .ok_or_else(|| format!("block not found at height {h}"))?;
        tx_count += block.transactions.len() as u64;

        // Rough byte count: header (~1.5KB) + txs.
        bytes_read += 1500;
        for tx in block.transactions.iter() {
            bytes_read += tx
                .zcash_serialize_to_vec()
                .map(|v| v.len() as u64)
                .unwrap_or(0);
        }

        if h % 10_000 == 0 {
            let elapsed = start_time.elapsed().as_secs_f64();
            let mbs = bytes_read as f64 / elapsed / 1_000_000.0;
            info!(
                height = h,
                tx_total = tx_count,
                mbytes = bytes_read / 1_000_000,
                elapsed_secs = elapsed,
                throughput_mb_s = format!("{mbs:.1}"),
                "scan progress"
            );
        }
    }

    let elapsed = start_time.elapsed().as_secs_f64();
    info!(
        end = finalized_end,
        mbytes = bytes_read / 1_000_000,
        tx_total = tx_count,
        elapsed_secs = elapsed,
        throughput_mb_s = format!("{:.1}", bytes_read as f64 / elapsed / 1_000_000.0),
        "scan complete"
    );

    Ok(())
}

fn bootstrap(
    db_path: &Path,
    zebra_db_dir: &Path,
    config_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_bootstrap_config(config_path)?;

    let zebra_network = cfg.network.to_zebra_network();
    let net_dir = zebra_network.lowercase_name();

    let default_start = NetworkUpgrade::Sapling
        .activation_height(&zebra_network)
        .expect("Sapling activation height must be set")
        .0;
    let start_height = cfg.start_height.unwrap_or(default_start);

    // Derive block store path: config's storage.database.path, or config dir,
    // or CLI --db-path.
    let block_store_path = resolve_block_store_path(config_path, &cfg);
    let block_store_path = if db_path == Path::new("data/lmdb") {
        // --db-path not overridden on CLI — use config-derived path.
        block_store_path
    } else {
        db_path.join("block_store")
    };
    let sapling_activation = default_start;
    let nu5_activation = NetworkUpgrade::Nu5
        .activation_height(&zebra_network)
        .map(|h| h.0);

    // Open Zebra RocksDB via zebra-state's typed wrapper.
    let zebra_config = ZebraConfig {
        cache_dir: zebra_db_dir.to_path_buf(),
        ..Default::default()
    };
    let db_path_str = zebra_config.db_path("state", 27, &zebra_network);
    info!(
        path = %db_path_str.display(),
        network = %net_dir,
        start_height,
        "opening Zebra state database"
    );
    let zebra_db = zebra_state::ZebraDb::new(
        &zebra_config,
        "state",
        &zebra_state::state_database_format_version_in_code(),
        &zebra_network,
        false, // debug_skip_format_upgrades
        STATE_COLUMN_FAMILIES.iter().map(|s| s.to_string()),
        true, // read_only
    );

    // Get tip and compute finalized range.
    let (tip_height, _tip_hash) = zebra_db
        .tip()
        .ok_or("Zebra state database is empty — no tip found")?;
    let finalized_end = tip_height.0.saturating_sub(MAX_REORG_DEPTH);

    if start_height > finalized_end {
        return Err(format!(
            "start_height ({start_height}) is above finalized tip \
             ({} - {MAX_REORG_DEPTH} = {finalized_end})",
            tip_height.0,
        )
        .into());
    }

    info!(
        tip = tip_height.0,
        start = start_height,
        end = finalized_end,
        count = finalized_end.saturating_sub(start_height).wrapping_add(1),
        "bootstrap range"
    );

    // Open output LMDB.
    info!(path = %block_store_path.display(), "output LMDB");
    fs::create_dir_all(&block_store_path)?;
    let lmdb = LmdbStore::open(&block_store_path)?;

    if let Some(existing) = lmdb.block_count()?.and_then(|c| c.checked_sub(1)) {
        if existing >= start_height {
            return Err(format!(
                "LMDB already has blocks up to height {existing}. \
                 Delete the block_store directory first to start fresh."
            )
            .into());
        }
    }

    let network = zebra_network.clone();
    let total = finalized_end.saturating_sub(start_height).wrapping_add(1) as usize;

    // Chunked parallel reads. Atomic cursor within each chunk ensures
    // work-stealing across threads — large blocks never starve others.
    const CHUNK_SIZE: u32 = 50_000;

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    info!(num_threads, chunk_size = CHUNK_SIZE, "bootstrap work queue");

    let zebra_db = Arc::new(zebra_db);
    let network = &network;
    let mut chunk_start = start_height;
    const FLUSH_BATCH: usize = 10_000;

    while chunk_start <= finalized_end {
        let chunk_end = std::cmp::min(chunk_start + CHUNK_SIZE - 1, finalized_end);
        let chunk_total = chunk_end - chunk_start + 1;

        let cursor = AtomicU32::new(chunk_start);
        let results: Mutex<Vec<(u32, [u8; 32], StoreBlock)>> =
            Mutex::new(Vec::with_capacity(chunk_total as usize));

        thread::scope(|s| {
            for _ in 0..num_threads {
                let cursor = &cursor;
                let results = &results;
                let zebra_db = Arc::clone(&zebra_db);
                let network = network.clone();
                s.spawn(move || loop {
                    let h = cursor.fetch_add(1, Ordering::Relaxed);
                    if h > chunk_end {
                        break;
                    }
                    let height = zebra_chain::block::Height(h);
                    let block = zebra_db
                        .block(zebra_state::HashOrHeight::Height(height))
                        .unwrap_or_else(|| panic!("block not found at height {h}"));
                    let sapling_tree = if h >= sapling_activation {
                        zebra_db.sapling_tree_by_height(&height)
                    } else {
                        None
                    }
                    .unwrap_or_default();
                    let orchard_tree = if nu5_activation.is_some_and(|nu5| h >= nu5) {
                        zebra_db.orchard_tree_by_height(&height)
                    } else {
                        None
                    }
                    .unwrap_or_default();
                    // Conversion with zero chainwork — chainwork is not stored
                    // in CompactBlock output, so placeholder is fine.
                    let metadata = BlockMetadata::new(
                        sapling_tree.root(),
                        sapling_tree.count() as u32,
                        orchard_tree.root(),
                        orchard_tree.count() as u32,
                        ChainWork::from_u256(primitive_types::U256::zero()),
                        network.clone(),
                    );
                    let indexed = IndexedBlock::try_from(BlockWithMetadata::new(&block, metadata))
                        .unwrap_or_else(|e| panic!("IndexedBlock::try_from at height {h}: {e}"));
                    let compact_bytes = prost::Message::encode_to_vec(&indexed.to_compact_block());
                    results.lock().unwrap().push((
                        h,
                        block.hash().0,
                        StoreBlock::new(h, block.hash().0, block.header.previous_block_hash.0, compact_bytes),
                    ));
                });
            }
        });

        let mut fetched = results.into_inner().unwrap();
        fetched.sort_by_key(|(h, ..)| *h);

        let mut batch: Vec<([u8; 32], StoreBlock)> = Vec::with_capacity(FLUSH_BATCH);
        for (h, hash, block) in fetched {
            batch.push((hash, block));
            if batch.len() >= FLUSH_BATCH {
                lmdb.put_batch(&batch)?;
                info!(
                    height = h,
                    batch_size = batch.len(),
                    progress = format!(
                        "{:.1}%",
                        (h - start_height + 1) as f64 / total as f64 * 100.0
                    ),
                    "flushed batch to LMDB"
                );
                batch.clear();
            }
        }
        if !batch.is_empty() {
            lmdb.put_batch(&batch)?;
            info!(count = batch.len(), "flushed final batch to LMDB");
        }
        chunk_start = chunk_end + 1;
    }

    let final_height = lmdb.block_count()?.map(|c| c - 1);
    info!(
        lmdb_tip = final_height,
        "bootstrap complete. Run zainod to resume sync."
    );

    Ok(())
}
