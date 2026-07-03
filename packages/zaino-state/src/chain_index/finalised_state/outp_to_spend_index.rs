//! Standalone, parallel-buildable finalised spend index: maps each transparent
//! outpoint to the txid of the transaction that consumed it.
//!
//! Proof of concept, gated behind the `outp_to_spend_index` feature. See
//! `docs/adr/0006-finalised-spend-index-parallel-build.md`.
//!
//! The build stages live here: **extract** spends from a block batch,
//! **collate** them into LMDB key order, and bulk-load them into the index's
//! own LMDB store via `MDB_APPEND` ([`SpendIndexDb`]). The independent,
//! move-only sync loop ([`SpendIndexSync`]) drives these in one shot over
//! `[start_height, finalised_tip]` streamed from a [`SpendIndexSource`]:
//! block sourcing/extraction fans out across worker tasks pulling fixed-size
//! height chunks from a shared queue (`workers = 1` is the serial baseline —
//! the same path minus concurrency, so serial and parallel builds compare
//! stage-for-stage), and collation stays one global sort feeding one
//! `MDB_APPEND` pass. [`spawn_build`] wires it onto the owning `ChainIndex`
//! (StateService only, from the Sapling activation height). (Table-level
//! integrity over the entries is deferred; it is not MVP.)

use std::path::Path;

use lmdb::{Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction as _, WriteFlags};

use crate::chain_index::finalised_state::build_indexed_block_from_source;
use crate::chain_index::source::validator_connector::StateSource;
use crate::chain_index::source::BlockchainSource;
use crate::chain_index::types::TransactionHash;
use crate::chain_index::OPERATIONAL_NFS_DEPTH;
use crate::config::ChainIndexConfig;
use crate::error::FinalisedStateError;
use crate::{IndexedBlock, Outpoint, TransparentCompactTx, ZainoVersionedSerde as _};
use zaino_common::Network;
use zebra_chain::parameters::NetworkUpgrade;

/// One transparent spend: the consumed outpoint paired with the txid of the
/// transaction that consumed it.
pub(super) type SpendRecord = (Outpoint, TransactionHash);

/// A spend entry encoded for storage: the LMDB key (the encoded outpoint) and
/// value (the bare 32-byte spending txid), ready for an `MDB_APPEND` load.
pub(super) type EncodedSpend = (Vec<u8>, [u8; 32]);

// ── Extract ──────────────────────────────────────────────────────────────────

/// Extracts every transparent spend recorded in `blocks`.
///
/// Pure and statically read-free: it is handed only block data — no database
/// and no validator handle — so a previous-output lookup is unrepresentable,
/// not merely discouraged. Each transparent input yields
/// `(prevout_outpoint, spending_txid)` directly, where the spending txid is the
/// containing transaction's own id; nothing outside the block stream is
/// consulted.
///
/// TODO (loop slice): add a buffer-filling variant that writes into a
/// caller-owned `&mut Vec<SpendRecord>`, once the per-worker loop exists to
/// reuse one allocation across batches (`Vec::clear` keeps capacity ⇒ ~zero
/// steady-state alloc, and the collator can sort that buffer in place).
/// Deferred until a real reusing caller anchors it: the extractor is dwarfed by
/// zebra block I/O and the sort, so buffer reuse only pays then.
pub(super) fn extract_spends(blocks: &[IndexedBlock]) -> Vec<SpendRecord> {
    blocks
        .iter()
        .flat_map(|block| block.transactions().iter())
        .flat_map(|tx| spends_in_transaction(*tx.txid(), tx.transparent()))
        .collect()
}

/// The spends contributed by one transaction: each outpoint it consumes paired
/// with `spending_txid`. The non-coinbase filtering lives in
/// [`TransparentCompactTx::spent_outpoints`]; this only attaches the spender.
fn spends_in_transaction(
    spending_txid: TransactionHash,
    transparent: &TransparentCompactTx,
) -> impl Iterator<Item = SpendRecord> + '_ {
    transparent
        .spent_outpoints()
        .map(move |outpoint| (outpoint, spending_txid))
}

// ── Collate ──────────────────────────────────────────────────────────────────

/// Encodes and sorts `records` into LMDB key order — byte-wise on the encoded
/// outpoint key, matching LMDB's default comparator — so the result can be
/// bulk-loaded with `MDB_APPEND`.
///
/// Spend keys are globally disjoint — each outpoint is spent at most once on a
/// chain — so this is a pure sort needing no cross-record reconciliation; a
/// duplicate key means corrupt input and is rejected.
pub(super) fn collate(records: &[SpendRecord]) -> Result<Vec<EncodedSpend>, FinalisedStateError> {
    let mut encoded = records
        .iter()
        .map(|(outpoint, spending_txid)| {
            Ok((outpoint.to_bytes()?, <[u8; 32]>::from(*spending_txid)))
        })
        .collect::<Result<Vec<EncodedSpend>, FinalisedStateError>>()?;

    encoded.sort_by(|a, b| a.0.cmp(&b.0));

    if let Some(duplicate) = encoded.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(FinalisedStateError::Custom(format!(
            "duplicate spend-index key during collation: {:?}",
            duplicate[0].0
        )));
    }

    Ok(encoded)
}

// ── Store ────────────────────────────────────────────────────────────────────

/// LMDB database name for the spend index (versioned, mirroring the
/// finalised-state naming convention).
const SPEND_DB_NAME: &str = "outp_to_spend_index_1_0_0";

/// The spend index's **own** LMDB store: a single database keyed by the encoded
/// outpoint, valued by the bare 32-byte spending txid. Standalone — its own
/// `Environment`, independent of the finalised-state monolith's database.
pub(super) struct SpendIndexDb {
    env: Environment,
    spend: Database,
}

impl SpendIndexDb {
    /// Opens (creating if absent) the spend index at `path` with the given LMDB
    /// `map_size`. Mirrors the finalised-state environment: **no `WRITE_MAP`**
    /// (a crash discards uncommitted data rather than corrupting), with
    /// reader-friendly flags. `path` is a directory, created if missing.
    pub(super) fn open(path: &Path, map_size: usize) -> Result<Self, FinalisedStateError> {
        std::fs::create_dir_all(path).map_err(|error| {
            FinalisedStateError::Custom(format!(
                "spend index: create dir {}: {error}",
                path.display()
            ))
        })?;
        let env = Environment::new()
            .set_max_dbs(1)
            .set_map_size(map_size)
            .set_flags(EnvironmentFlags::NO_TLS | EnvironmentFlags::NO_READAHEAD)
            .open(path)?;
        let spend = env.create_db(Some(SPEND_DB_NAME), DatabaseFlags::empty())?;
        Ok(Self { env, spend })
    }

    /// Bulk-loads collated (sorted, disjoint-key) entries with `MDB_APPEND` — a
    /// sequential B-tree fill — in one transaction, then forces durability.
    /// `entries` must be in ascending key order (see [`collate`]); a key out of
    /// order or already present makes LMDB reject the append.
    pub(super) fn bulk_load(&self, entries: &[EncodedSpend]) -> Result<(), FinalisedStateError> {
        // One-shot / global-order contract: `entries` must be strictly ascending
        // by key — `collate`'s output for *all* spends at once. `MDB_APPEND`
        // requires every key to exceed the current maximum, so there is no
        // re-sort across calls: a second `bulk_load` would need every key to
        // exceed the first call's maximum. LMDB self-checks this at runtime (it
        // rejects a non-increasing key with `KeyExist`); this assert fails earlier
        // and with a clearer message in debug builds if a caller hands over an
        // unsorted, duplicate, or per-batch (not globally sorted) input.
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "bulk_load: entries must be strictly ascending, globally-sorted keys \
             (MDB_APPEND is one-shot — feed all spends through a single collate)",
        );
        let mut txn = self.env.begin_rw_txn()?;
        for (key, value) in entries {
            txn.put(self.spend, key, value, WriteFlags::APPEND)?;
        }
        txn.commit()?;
        self.env.sync(true)?;
        Ok(())
    }

    /// The txid that consumed `outpoint`, or `None` if it is unspent in
    /// finalised state. A single point lookup. The serve-time union with the
    /// non-finalised window already exists as
    /// `ChainIndex::get_outpoint_spenders` (`ChainScope::FullChain`, #1167);
    /// wiring this store in as that method's finalised leg — replacing the
    /// monolith's `spent` table + `TxLocation → txid` resolution — is the
    /// deferred serving step.
    pub(super) fn spending_txid(
        &self,
        outpoint: &Outpoint,
    ) -> Result<Option<TransactionHash>, FinalisedStateError> {
        let key = outpoint.to_bytes()?;
        let ro = self.env.begin_ro_txn()?;
        match ro.get(self.spend, &key) {
            Ok(bytes) => {
                let array: [u8; 32] = bytes.try_into().map_err(|_| {
                    FinalisedStateError::Custom(
                        "spend index: stored value is not a 32-byte txid".to_string(),
                    )
                })?;
                Ok(Some(TransactionHash::from(array)))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

// ── Sync loop ────────────────────────────────────────────────────────────────

/// The sources the spend-index POC may build from: zebra's StateService
/// ([`StateSource`]) and the test mockchain — **never** the JSON-RPC/zcashd
/// `FetchService`. Binding [`SpendIndexSync`] to `S: SpendIndexSource` makes
/// "never FetchService" a compile-time fact: the `Fetch` path does not
/// implement this trait.
pub(super) trait SpendIndexSource: BlockchainSource {}

impl SpendIndexSource for StateSource {}

#[cfg(test)]
impl SpendIndexSource for crate::chain_index::source::mockchain_source::MockchainSource {}

/// Stage timings and volumes from one one-shot build: the benchmark's
/// measurement surface. `stream_and_extract` covers the sequential
/// fetch → decode → extract loop — the stage a parallel build transforms —
/// while `collate` and `bulk_load` are identical across build variants, so
/// comparing two builds' stats isolates the streaming stage.
#[derive(Debug, Default, Clone)]
pub(crate) struct SpendIndexBuildStats {
    /// Worker tasks the streaming stage fanned out across (`1` = the serial
    /// baseline), recorded so every measurement is self-describing.
    pub(crate) workers: u32,
    /// Blocks fetched and scanned.
    pub(crate) blocks: u32,
    /// Spend records extracted (= entries loaded).
    pub(crate) spends: usize,
    /// Wall-clock of the fetch → decode → extract loop.
    pub(crate) stream_and_extract: std::time::Duration,
    /// Wall-clock of the global encode + sort + duplicate check.
    pub(crate) collate: std::time::Duration,
    /// Wall-clock of the `MDB_APPEND` load and durability sync.
    pub(crate) bulk_load: std::time::Duration,
}

/// The independent, single-run builder for the spend index.
///
/// Move-only (`!Clone`) with a private constructor and a `self`-consuming
/// [`run`](Self::run): the type system makes a second concurrent build
/// unrepresentable — "only one loop at a time" without a runtime guard. (The
/// guarantee constrains *builds*; the fan-out of worker tasks inside one
/// `run` is not a second build.) POC build model: re-stream `start_height` →
/// finalised tip from `source` across `workers` chunk-pulling tasks, extract
/// every spend, collate once, and bulk-load the index in a single
/// `MDB_APPEND` pass.
pub(super) struct SpendIndexSync<S: SpendIndexSource> {
    source: S,
    db: SpendIndexDb,
    /// First height to index, inclusive. Genesis (`0`) for a full index, or a
    /// higher height — e.g. a network-upgrade activation — to cover only spends
    /// occurring from that epoch onward.
    start_height: u32,
    /// Benchmark knob: when set, the build stops at
    /// `min(finalised_tip, cap)` instead of the finalised tip, so a baseline
    /// run can cover a fixed block range. `None` (production) builds to the
    /// finalised tip.
    end_height_cap: Option<u32>,
    /// Worker tasks for the streaming stage. Defaults to the machine's
    /// available parallelism; `1` selects the serial baseline (the identical
    /// path minus concurrency), which is what parallel runs are measured
    /// against.
    workers: usize,
}

impl<S: SpendIndexSource> SpendIndexSync<S> {
    /// Private mint: production enters via [`SpendIndexSync::from_state`]
    /// (StateService only); tests construct directly with a `MockchainSource`.
    fn new(source: S, db: SpendIndexDb, start_height: u32) -> Self {
        Self {
            source,
            db,
            start_height,
            end_height_cap: None,
            workers: std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        }
    }

    /// Builds the spend index from its start height to the finalised tip,
    /// consuming the handle and returning the per-stage timings. One-shot
    /// (POC): no resume, no batching — a single global `collate` +
    /// `MDB_APPEND`.
    pub(super) async fn run(self) -> Result<SpendIndexBuildStats, FinalisedStateError> {
        let Some(best) = self.source.get_best_block_height().await.map_err(|error| {
            FinalisedStateError::Custom(format!("spend index: fetch best height: {error:?}"))
        })?
        else {
            return Ok(SpendIndexBuildStats::default()); // empty chain — nothing finalised to index
        };

        // Finalised tip: below the reorg-possible window, clamped at genesis.
        // A benchmark cap lowers it further so a run covers a fixed range.
        let finalised_tip = best.0.saturating_sub(OPERATIONAL_NFS_DEPTH);
        let finalised_tip = self
            .end_height_cap
            .map_or(finalised_tip, |cap| finalised_tip.min(cap));

        // Stream finalised blocks across the workers, extracting spends and
        // dropping each block; only the (smaller) spend records are retained
        // for the one-shot sort. Fixed-size chunks pulled from a shared queue
        // self-balance the block-weight skew across the chain (late blocks
        // are far heavier than early ones), so equal-width per-worker ranges
        // are not needed.
        let workers = self.workers.max(1);
        let chunks =
            std::sync::Arc::new(chunk_ranges(self.start_height, finalised_tip, CHUNK_BLOCKS));
        let next_chunk = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut stats = SpendIndexBuildStats {
            workers: workers as u32,
            blocks: chunks.iter().map(|(lo, hi)| hi - lo + 1).sum(),
            ..Default::default()
        };

        let stream_started = std::time::Instant::now();
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(tokio::spawn(extract_chunks(
                self.source.clone(),
                std::sync::Arc::clone(&chunks),
                std::sync::Arc::clone(&next_chunk),
            )));
        }
        let mut spends: Vec<SpendRecord> = Vec::new();
        for handle in handles {
            let worker_records = handle.await.map_err(|error| {
                FinalisedStateError::Custom(format!("spend index: worker task died: {error}"))
            })??;
            spends.extend(worker_records);
        }
        stats.stream_and_extract = stream_started.elapsed();
        stats.spends = spends.len();

        let collate_started = std::time::Instant::now();
        let collated = collate(&spends)?;
        stats.collate = collate_started.elapsed();

        let load_started = std::time::Instant::now();
        tokio::task::block_in_place(|| self.db.bulk_load(&collated))?;
        stats.bulk_load = load_started.elapsed();

        Ok(stats)
    }
}

/// Blocks per work-queue chunk. Small relative to any real build range so
/// chunk-pulling self-balances the block-weight skew, large enough that queue
/// bookkeeping (one relaxed `fetch_add` per chunk) is noise.
const CHUNK_BLOCKS: u32 = 1_000;

/// Splits inclusive `[start, end]` into consecutive inclusive ranges of at
/// most `size` blocks. Empty when `start > end`.
fn chunk_ranges(start: u32, end: u32, size: u32) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut lo = start;
    while lo <= end {
        let hi = lo.saturating_add(size - 1).min(end);
        ranges.push((lo, hi));
        match hi.checked_add(1) {
            Some(next) => lo = next,
            None => break, // end == u32::MAX: the final range is pushed
        }
    }
    ranges
}

/// One streaming worker: pulls chunk indices from the shared queue until it
/// drains, fetching each height and extracting its spends into a
/// worker-local buffer (returned for the caller to concatenate).
///
/// Roots-free: the per-block cost is one `get_block` plus a
/// transparent-input walk — no commitment-tree-root fetch and no compact
/// conversion of the (discarded) shielded data. The monolith's per-block
/// ingestion awaits `get_block` *and* `get_commitment_tree_roots` in
/// sequence, the fetch-latency pattern PR #1241 measured collapsing to
/// ~1 blk/s in the sandblast band; the spend index never needs the second
/// call.
async fn extract_chunks<S: SpendIndexSource>(
    source: S,
    chunks: std::sync::Arc<Vec<(u32, u32)>>,
    next_chunk: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Result<Vec<SpendRecord>, FinalisedStateError> {
    let mut records = Vec::new();
    loop {
        let index = next_chunk.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(&(lo, hi)) = chunks.get(index) else {
            return Ok(records);
        };
        for height in lo..=hi {
            let block = source
                .get_block(zebra_state::HashOrHeight::Height(
                    zebra_chain::block::Height(height),
                ))
                .await?
                .ok_or_else(|| {
                    FinalisedStateError::Custom(format!(
                        "spend index: no block at finalised height {height}"
                    ))
                })?;
            records.extend(extract_spends_from_zebra_block(&block));
        }
    }
}

/// Extracts every transparent spend in one zebra block: each non-coinbase
/// input yields `(prevout_outpoint, containing_txid)`. Statically read-free —
/// handed only block data — like [`extract_spends`], its compact-form twin;
/// the sync-loop tests assert both paths agree over the same chains.
fn extract_spends_from_zebra_block(
    block: &zebra_chain::block::Block,
) -> impl Iterator<Item = SpendRecord> + '_ {
    block.transactions.iter().flat_map(|tx| {
        let spending_txid = TransactionHash::from(tx.hash().0);
        tx.inputs().iter().filter_map(move |input| {
            input
                .outpoint()
                .map(|prevout| (Outpoint::new(prevout.hash.0, prevout.index), spending_txid))
        })
    })
}

/// Map size for the spend index's own LMDB env. LMDB grows the file lazily (no
/// `WRITE_MAP` here), so this is an upper bound, not a preallocation.
const SPEND_INDEX_MAP_SIZE: usize = 32 * 1024 * 1024 * 1024;

/// The spend index's on-disk location: a sibling of the configured chain-index
/// database directory.
fn spend_index_dir(cfg: &ChainIndexConfig) -> std::path::PathBuf {
    let main = &cfg.storage.database.path;
    match main.parent() {
        Some(parent) => parent.join("outp_to_spend_index"),
        None => main.join("outp_to_spend_index"),
    }
}

/// Spawns the one-shot finalised spend-index build as a background task.
///
/// The build streams `[Sapling activation, finalised tip]` from `source` — the
/// zebra StateService, supplied by
/// [`BlockchainSource::finalised_spend_index_source`] — into the index's own
/// LMDB env. The task logs its outcome; the owning `ChainIndex` holds the
/// returned handle and aborts it on shutdown/drop.
pub(crate) fn spawn_build(
    source: StateSource,
    cfg: &ChainIndexConfig,
) -> tokio::task::JoinHandle<Result<SpendIndexBuildStats, FinalisedStateError>> {
    let network = cfg.network;
    let dir = spend_index_dir(cfg);
    tokio::spawn(async move {
        let start_height = NetworkUpgrade::Sapling
            .activation_height(&network.to_zebra_network())
            .ok_or_else(|| {
                FinalisedStateError::Custom("network has no Sapling activation height".to_string())
            })?
            .0;
        let db = SpendIndexDb::open(&dir, SPEND_INDEX_MAP_SIZE)?;
        let mut sync = SpendIndexSync::new(source, db, start_height);
        if let Some(start) = parsed_env::<u32>(BENCH_START_HEIGHT_ENV) {
            tracing::warn!(
                start,
                "spend-index build start overridden via {BENCH_START_HEIGHT_ENV} (benchmark knob)",
            );
            sync.start_height = start;
        }
        if let Some(cap) = parsed_env::<u32>(BENCH_END_HEIGHT_ENV) {
            tracing::warn!(
                cap,
                "spend-index build height-capped via {BENCH_END_HEIGHT_ENV} (benchmark knob)",
            );
            sync.end_height_cap = Some(cap);
        }
        if let Some(workers) = parsed_env::<usize>(WORKERS_ENV) {
            tracing::warn!(
                workers,
                "spend-index build worker count set via {WORKERS_ENV} (1 = serial baseline)",
            );
            sync.workers = workers.max(1);
        }
        let result = sync.run().await;
        match &result {
            Ok(stats) => tracing::info!(?stats, "finalised spend-index build complete"),
            Err(error) => tracing::error!("finalised spend-index build failed: {error}"),
        }
        result
    })
}

/// Benchmark env knobs. Each is ignored when unset and announced with a
/// `warn!` when active, so a bounded or tuned build can't pass silently for
/// a production one. Start/end bound the built range (e.g. a fixed 100k-block
/// window); the worker knob selects the streaming fan-out, with `1` the
/// serial baseline that parallel runs are measured against.
const BENCH_START_HEIGHT_ENV: &str = "ZAINO_SPEND_INDEX_START_HEIGHT";
const BENCH_END_HEIGHT_ENV: &str = "ZAINO_SPEND_INDEX_END_HEIGHT";
const WORKERS_ENV: &str = "ZAINO_SPEND_INDEX_WORKERS";

/// The parsed value of env knob `name`, or `None` when unset or unparseable
/// (the latter is logged, not fatal — a benchmark knob must not take the
/// production build down).
fn parsed_env<T: std::str::FromStr>(name: &str) -> Option<T>
where
    T::Err: std::fmt::Display,
{
    let raw = std::env::var(name).ok()?;
    match raw.parse() {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(%raw, "ignoring unparseable {name}: {error}");
            None
        }
    }
}

#[cfg(test)]
mod spends_in_transaction {
    use super::*;
    use crate::TxInCompact;

    /// Arbitrary fill byte for the spending transaction's txid, kept distinct
    /// from the prevout txids below so the two can't be confused.
    const SPENDER_TXID_BYTE: u8 = 9;

    fn txid(byte: u8) -> TransactionHash {
        TransactionHash::from([byte; 32])
    }

    #[test]
    fn skips_coinbase_input_keeps_real_spends() {
        let spender = txid(SPENDER_TXID_BYTE);
        let transparent = TransparentCompactTx::new(
            vec![
                TxInCompact::null_prevout(),    // coinbase input → contributes nothing
                TxInCompact::new([1u8; 32], 0), // spends output 0 of txid 0x01..
                TxInCompact::new([2u8; 32], 7), // spends output 7 of txid 0x02..
            ],
            vec![],
        );

        let records: Vec<SpendRecord> =
            super::spends_in_transaction(spender, &transparent).collect();

        assert_eq!(
            records,
            vec![
                (Outpoint::new([1u8; 32], 0), spender),
                (Outpoint::new([2u8; 32], 7), spender),
            ]
        );
    }
}

#[cfg(test)]
mod collate {
    use super::*;

    fn record(outpoint_byte: u8, index: u32) -> SpendRecord {
        (
            Outpoint::new([outpoint_byte; 32], index),
            TransactionHash::from([0xaau8; 32]),
        )
    }

    #[test]
    fn sorts_into_ascending_key_order() {
        let records = [record(3, 0), record(1, 0), record(2, 0)];
        let encoded = super::collate(&records).expect("disjoint keys collate cleanly");
        assert!(
            encoded.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "collated keys must be strictly ascending for MDB_APPEND",
        );
    }

    #[test]
    fn rejects_duplicate_keys() {
        let records = [record(1, 0), record(1, 0)];
        assert!(super::collate(&records).is_err());
    }

    #[test]
    fn little_endian_index_tie_break_matches_byte_order() {
        // Same creating txid, two output indices. The index is LE-encoded, so
        // 256 (bytes `00 01 00 00`) sorts before 1 (`01 00 00 00`) under memcmp
        // — exactly the order LMDB's default comparator (and thus MDB_APPEND)
        // requires. collate must reproduce that, not a numeric ordering.
        let encoded =
            super::collate(&[record(7, 1), record(7, 256)]).expect("disjoint keys collate cleanly");

        assert!(
            encoded[0].0 < encoded[1].0,
            "keys must be strictly ascending for MDB_APPEND",
        );
        // The trailing LE u32 of each key shows index 256 precedes index 1.
        assert_eq!(
            &encoded[0].0[encoded[0].0.len() - 4..],
            &256u32.to_le_bytes()[..]
        );
        assert_eq!(
            &encoded[1].0[encoded[1].0.len() - 4..],
            &1u32.to_le_bytes()[..]
        );
    }
}

#[cfg(test)]
mod chunk_ranges {
    #[test]
    fn covers_the_range_exactly_once_with_ragged_tail() {
        assert_eq!(
            super::chunk_ranges(10, 34, 10),
            vec![(10, 19), (20, 29), (30, 34)],
        );
    }

    #[test]
    fn exact_multiple_has_no_ragged_tail() {
        assert_eq!(super::chunk_ranges(0, 19, 10), vec![(0, 9), (10, 19)]);
    }

    #[test]
    fn single_block_range_is_one_chunk() {
        assert_eq!(super::chunk_ranges(7, 7, 10), vec![(7, 7)]);
    }

    #[test]
    fn empty_when_start_exceeds_end() {
        assert!(super::chunk_ranges(8, 7, 10).is_empty());
    }

    #[test]
    fn end_at_u32_max_terminates() {
        let ranges = super::chunk_ranges(u32::MAX - 5, u32::MAX, 4);
        assert_eq!(
            ranges,
            vec![(u32::MAX - 5, u32::MAX - 2), (u32::MAX - 1, u32::MAX)],
        );
    }
}

#[cfg(test)]
mod spend_index_db {
    use super::*;

    #[test]
    fn bulk_load_then_point_read_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "outp_to_spend_index_roundtrip_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let db = SpendIndexDb::open(&dir, 1 << 20).expect("open spend index");

        let spent_a = Outpoint::new([1u8; 32], 0);
        let spent_b = Outpoint::new([2u8; 32], 7);
        let spender = TransactionHash::from([9u8; 32]);

        let collated = super::collate(&[(spent_a, spender), (spent_b, spender)]).expect("collate");
        db.bulk_load(&collated).expect("bulk load");

        assert_eq!(db.spending_txid(&spent_a).expect("read a"), Some(spender));
        assert_eq!(db.spending_txid(&spent_b).expect("read b"), Some(spender));
        // An outpoint with no recorded spend reads back as unspent.
        let unspent = Outpoint::new([3u8; 32], 0);
        assert_eq!(db.spending_txid(&unspent).expect("read unspent"), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod spend_index_sync {
    use super::*;
    use crate::chain_index::source::mockchain_source::MockchainSource;
    use crate::chain_index::tests::vectors::{
        build_active_mockchain_source, indexed_block_chain, load_test_vectors, TestVectorBlockData,
    };

    fn fixture_blocks() -> Vec<TestVectorBlockData> {
        load_test_vectors().expect("load test vectors").blocks
    }

    /// Builds the spend index from `mock` into a fresh temp dir, returning a
    /// reader handle to what was persisted plus the dir to clean up. `run`
    /// consumes its writer handle, so the db is reopened for reading.
    ///
    /// `multi_thread` is required of the callers: `run` uses
    /// `tokio::task::block_in_place` for the LMDB write, which panics on a
    /// current-thread runtime. The network only drives the block builder's
    /// commitment-root activation checks (the mockchain supplies roots
    /// regardless) and is irrelevant to transparent-spend extraction.
    async fn build_index(mock: MockchainSource, tag: &str) -> (SpendIndexDb, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("outp_to_spend_index_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = SpendIndexDb::open(&dir, 1 << 24).expect("open spend index");
        SpendIndexSync::new(mock, db, 0)
            .run()
            .await
            .expect("spend-index build runs");
        (
            SpendIndexDb::open(&dir, 1 << 24).expect("reopen spend index"),
            dir,
        )
    }

    /// `run` must index *exactly* the transparent spends in the finalised range
    /// `0..=finalised_tip`: every finalised spend mapped to its spender, every
    /// non-finalised spend absent.
    ///
    /// Coverage note: the 201-block fixture caps `get_best_block_height` at 200
    /// (the mockchain reveals loaded blocks, it cannot mint new ones), so at the
    /// `#[cfg(test)]` depth of 100 the finalised tip is pinned at 100. Regtest
    /// coinbase maturity (100 blocks) puts every fixture spend at height ≥101, so
    /// the finalised range holds no spends and this test exercises the exclusion
    /// direction end to end (fetch → finalised boundary → empty collate → LMDB →
    /// read-back). Finalised-spend *mapping* with real records is covered by the
    /// `spend_index_db` round-trip test; an end-to-end presence check needs a
    /// longer synthetic chain (tracked in zingolabs/zaino#1334).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_indexes_exactly_the_finalised_spends() {
        let blocks = fixture_blocks();
        let chain_top = blocks.last().expect("non-empty chain").height;
        // No mining: the mock's tip is its max loaded height, so this matches
        // `run`'s own `best - OPERATIONAL_NFS_DEPTH`.
        let finalised_tip = chain_top - OPERATIONAL_NFS_DEPTH;

        let mock = build_active_mockchain_source(chain_top, blocks.clone());
        let (db, dir) = build_index(mock, "run").await;

        let mut saw_non_finalised_spend = false;
        for block in indexed_block_chain(&blocks) {
            let finalised = block.context.index.height.0 <= finalised_tip;
            for (outpoint, spending_txid) in extract_spends(std::slice::from_ref(&block)) {
                let indexed = db.spending_txid(&outpoint).expect("read spend");
                if finalised {
                    assert_eq!(
                        indexed,
                        Some(spending_txid),
                        "finalised spend {outpoint:?} must be indexed to its spender",
                    );
                } else {
                    saw_non_finalised_spend = true;
                    assert_eq!(
                        indexed, None,
                        "non-finalised spend {outpoint:?} must not be indexed",
                    );
                }
            }
        }
        assert!(
            saw_non_finalised_spend,
            "fixture should contain spends above the seam to exercise exclusion",
        );

        // A never-spent outpoint reads back as unspent.
        assert_eq!(
            db.spending_txid(&Outpoint::new([0xff; 32], u32::MAX))
                .expect("read unspent"),
            None,
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// End-to-end **presence** test: `run` must build a *non-empty* index when the
/// finalised range contains transparent spends. The 201-block fixture can't
/// exercise this (coinbase maturity puts its spends above the depth-100 seam),
/// so this synthesises a chain with zebra's block generator, which — with
/// `allow_all_transparent_coinbase_spends` — produces transparent spends that
/// can land below the seam. Tracks zingolabs/zaino#1334.
#[cfg(test)]
mod spend_index_presence {
    use super::*;
    use crate::chain_index::source::mockchain_source::MockchainSource;
    use proptest::prelude::{Arbitrary, Strategy};
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    use std::sync::Arc;
    use zebra_chain::block::arbitrary::{
        allow_all_transparent_coinbase_spends, LedgerStateOverride,
    };
    use zebra_chain::block::{Block, Height};
    use zebra_chain::parameters::{Network as ZebraNetwork, GENESIS_PREVIOUS_BLOCK_HASH};
    use zebra_chain::transparent::Input;
    use zebra_chain::LedgerState;

    // Long enough that the depth-100 seam leaves room for finalised spends.
    const CHAIN_LEN: usize = 200;
    // Generate-until-found budget — a chain with a finalised transparent spend
    // is overwhelmingly likely within a couple of tries.
    const MAX_GEN_ATTEMPTS: usize = 40;

    fn build_mock(blocks: Vec<Arc<Block>>) -> MockchainSource {
        let hashes = blocks
            .iter()
            .map(|block| crate::BlockHash::from(block.hash()))
            .collect();
        // Synthetic blocks carry no commitment roots/treestates; with a
        // non-regtest network the block builder falls back to default roots, and
        // spends are root-independent anyway.
        let roots = vec![(None, None); blocks.len()];
        let treestates = vec![(Vec::new(), Vec::new()); blocks.len()];
        MockchainSource::new(blocks, roots, treestates, hashes)
    }

    /// True if a non-coinbase transparent input (a spend) occurs at or below
    /// `finalised_tip`. Block index equals height for a genesis-rooted chain.
    fn has_finalised_spend(blocks: &[Arc<Block>], finalised_tip: u32) -> bool {
        blocks.iter().take(finalised_tip as usize + 1).any(|block| {
            block.transactions.iter().any(|tx| {
                tx.inputs()
                    .iter()
                    .any(|i| matches!(i, Input::PrevOut { .. }))
            })
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_builds_a_nonempty_index_from_a_synthetic_chain() {
        let strategy = LedgerState::arbitrary_with(LedgerStateOverride {
            height_override: Some(Height(0)),
            previous_block_hash_override: Some(GENESIS_PREVIOUS_BLOCK_HASH),
            network_upgrade_override: None,
            transaction_version_override: None,
            transaction_has_valid_network_upgrade: true,
            always_has_coinbase: true,
            network_override: Some(ZebraNetwork::Mainnet),
        })
        .prop_flat_map(|ledger| {
            Block::partial_chain_strategy(
                ledger,
                CHAIN_LEN,
                allow_all_transparent_coinbase_spends,
                true,
            )
        });
        let mut runner = TestRunner::deterministic();

        // Deterministically search for a generated chain with a finalised spend,
        // so the presence direction is genuinely exercised.
        let mut found = None;
        for _ in 0..MAX_GEN_ATTEMPTS {
            let blocks = strategy
                .new_tree(&mut runner)
                .expect("generate chain")
                .current()
                .0;
            let finalised_tip = (blocks.len() as u32 - 1).saturating_sub(OPERATIONAL_NFS_DEPTH);
            if has_finalised_spend(&blocks, finalised_tip) {
                found = Some((blocks, finalised_tip));
                break;
            }
        }
        let (blocks, finalised_tip) =
            found.expect("a synthetic chain with a finalised transparent spend within budget");

        let mock = build_mock(blocks);

        // Reference: the spends in the finalised range, built the way `run`
        // builds blocks (so this checks fetch + boundary + collate + LMDB
        // round-trip; the extractor is unit-tested above).
        let zebra_network = Network::Mainnet.to_zebra_network();
        let sapling = NetworkUpgrade::Sapling
            .activation_height(&zebra_network)
            .expect("Sapling activation height");
        let nu5 = NetworkUpgrade::Nu5.activation_height(&zebra_network);
        let mut finalised_blocks = Vec::new();
        for height in 0..=finalised_tip {
            finalised_blocks.push(
                build_indexed_block_from_source(
                    &mock,
                    Network::Mainnet,
                    sapling,
                    nu5,
                    height,
                    None,
                )
                .await
                .expect("build finalised block"),
            );
        }
        let expected = extract_spends(&finalised_blocks);
        assert!(
            !expected.is_empty(),
            "the chosen chain must contain finalised transparent spends",
        );

        // Build the index from genesis and read it back.
        let dir = std::env::temp_dir().join(format!(
            "outp_to_spend_index_synthetic_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let db = SpendIndexDb::open(&dir, 1 << 24).expect("open spend index");
        SpendIndexSync::new(mock, db, 0)
            .run()
            .await
            .expect("spend-index build runs");
        let db = SpendIndexDb::open(&dir, 1 << 24).expect("reopen spend index");

        for (outpoint, spending_txid) in &expected {
            assert_eq!(
                db.spending_txid(outpoint).expect("read finalised spend"),
                Some(*spending_txid),
                "finalised spend {outpoint:?} must be indexed to its spender",
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
