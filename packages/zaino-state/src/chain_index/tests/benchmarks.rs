//! Performance benchmarks for the finalised-state write path.
//!
//! These are measurement harnesses, not correctness tests: each one times a
//! hot section of the block-write path and prints a throughput report, so a
//! before/after pair of runs quantifies an optimization. Every test is
//! `#[ignore]`d to keep it out of the default suite; run the set with:
//!
//! ```text
//! cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks
//! ```
//!
//! Coverage maps to the costs identified in the `write_block` review
//! (zingolabs/zaino#1207, threads r3398927655 and r3399020933):
//!
//! - [`write_block_chain_ingest`] — end-to-end `write_block` over the full
//!   regtest vector chain: per-tx indexing CPU plus the durable
//!   (fsynced) LMDB commit and post-commit validation read-back per block.
//! - [`stored_entry_fixed_encode`] — the `StoredEntryFixed::new(..).to_bytes()`
//!   pattern executed per transaction (`txid_location`) and per spent outpoint
//!   inside the write transaction.
//! - [`stored_entry_fixed_decode_verify`] — the read-back twin: `from_bytes`
//!   plus checksum `verify`, the per-entry cost of validation re-reads.
//! - [`stored_entry_var_encode`] — the `StoredEntryVar` encode used for the
//!   large per-block list values (txids, transparent, sapling, orchard).

use std::time::{Duration, Instant};

use crate::chain_index::finalised_state::capability::DbWrite as _;
use crate::chain_index::finalised_state::entry::{StoredEntryFixed, StoredEntryVar};
use crate::chain_index::finalised_state::write_batch::WriteBatcher;
use crate::chain_index::tests::finalised_state::v1::spawn_v1_zaino_db;
use crate::chain_index::tests::vectors::{
    build_mockchain_source, indexed_block_chain, load_test_vectors,
};
use crate::{IndexedBlock, TransactionHash, TxLocation, TxidList, ZainoVersionedSerde as _};

/// Sorts `runs` and prints min / median / mean plus median-based per-item time
/// and throughput for `items` work items per run.
fn report(label: &str, items: usize, unit: &str, runs: &mut [Duration]) {
    runs.sort_unstable();
    let min = runs[0];
    let median = runs[runs.len() / 2];
    let mean = runs.iter().sum::<Duration>() / runs.len() as u32;
    let per_item = median / items as u32;
    let throughput = items as f64 / median.as_secs_f64();
    println!(
        "[bench] {label}: {} runs x {items} {unit} — min {min:.2?}, median {median:.2?}, \
         mean {mean:.2?} | {per_item:.2?} per {unit}, {throughput:.0} {unit}/s",
        runs.len(),
    );
}

/// Deterministic pseudo-random 32-byte key for entry `i`. Mimics txid keys
/// (uniformly spread bytes) without pulling in an RNG, so runs are comparable.
fn synthetic_txid(i: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    for (chunk_index, chunk) in key.chunks_exact_mut(8).enumerate() {
        // splitmix64-style scramble; constants from the reference implementation.
        let mut x = i
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
            .wrapping_mul(chunk_index as u64 + 1);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        chunk.copy_from_slice(&(x ^ (x >> 31)).to_le_bytes());
    }
    key
}

/// End-to-end `write_block` ingest of the full regtest vector chain into a
/// fresh `ZainoDB` per run. Dominated by the durable LMDB commit (two fsyncs)
/// per block, with per-tx indexing and entry serialization on top — the
/// production sync-time profile.
///
/// multi_thread required: `DbV1::write_block` calls
/// `tokio::task::block_in_place`, which panics on a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run with `cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks`"]
async fn write_block_chain_ingest() {
    const RUNS: usize = 3;

    let test_vector_data = load_test_vectors().expect("test vectors load");
    let blocks = test_vector_data.blocks;
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();
    let block_count = chain.len();
    let tx_count: usize = chain.iter().map(|block| block.transactions().len()).sum();

    let mut runs = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let source = build_mockchain_source(blocks.clone());
        let (_db_dir, zaino_db) = spawn_v1_zaino_db(source).await.expect("spawn ZainoDB");
        let chain = chain.clone();

        let started = Instant::now();
        for block in chain {
            zaino_db
                .router()
                .write_block(block)
                .await
                .expect("write_block");
        }
        runs.push(started.elapsed());
        // `zaino_db` then `_db_dir` drop here: DB is torn down before its tempdir.
    }

    println!("[bench] vector chain: {block_count} blocks, {tx_count} transactions");
    report("write_block ingest", block_count, "blocks", &mut runs);
}

/// Batched end-to-end ingest: the same vector chain as
/// [`write_block_chain_ingest`], written through `WriteBatcher` →
/// `ZainoDB::write_blocks` so blocks share durable commits. Reported at two
/// budgets: one small enough to force several batches over this chain
/// (steady-state batching) and the production default (whole chain in one
/// commit — the upper bound). Compare against the per-block variant, ideally
/// with `TMPDIR` on a real filesystem where commits dominate.
///
/// multi_thread required: the write path calls `tokio::task::block_in_place`,
/// which panics on a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run with `cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks`"]
async fn write_block_chain_ingest_batched() {
    const RUNS: usize = 3;
    const SMALL_BUDGET: usize = 64 * 1024;

    let test_vector_data = load_test_vectors().expect("test vectors load");
    let blocks = test_vector_data.blocks;
    let chain: Vec<IndexedBlock> = indexed_block_chain(&blocks).collect();
    let block_count = chain.len();

    for budget in [
        SMALL_BUDGET,
        crate::chain_index::finalised_state::write_batch::DEFAULT_WRITE_BATCH_BYTE_BUDGET,
    ] {
        let mut runs = Vec::with_capacity(RUNS);
        let mut batches_per_run = 0;
        for _ in 0..RUNS {
            let source = build_mockchain_source(blocks.clone());
            let (_db_dir, zaino_db) = spawn_v1_zaino_db(source).await.expect("spawn ZainoDB");
            let chain = chain.clone();

            let started = Instant::now();
            let mut batcher = WriteBatcher::new(budget);
            let mut batches = 0;
            for block in chain {
                if let Some(batch) = batcher.push(block) {
                    zaino_db.write_blocks(&batch).await.expect("batched write");
                    batches += 1;
                }
            }
            if let Some(batch) = batcher.flush() {
                zaino_db
                    .write_blocks(&batch)
                    .await
                    .expect("final batched write");
                batches += 1;
            }
            runs.push(started.elapsed());
            batches_per_run = batches;
        }

        println!(
            "[bench] batched ingest: {block_count} blocks in {batches_per_run} batches \
             (budget {budget} bytes)"
        );
        report(
            &format!("write_blocks batched ingest (budget {budget})"),
            block_count,
            "blocks",
            &mut runs,
        );
    }
}

/// Per-entry encode cost of `StoredEntryFixed::new(key, item).to_bytes()` —
/// the exact pattern `write_block` runs per transaction (`txid_location`
/// table) and per spent outpoint (`spent` table). Sensitive to the
/// double-serialization in `new` + `to_bytes` and the checksum-input
/// concatenation.
#[test]
#[ignore = "benchmark: run with `cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks`"]
fn stored_entry_fixed_encode() {
    const ENTRIES: usize = 100_000;
    const RUNS: usize = 5;

    let entries: Vec<([u8; 32], TxLocation)> = (0..ENTRIES as u64)
        .map(|i| {
            (
                synthetic_txid(i),
                TxLocation::new((i / 1_000) as u32, (i % 1_000) as u16),
            )
        })
        .collect();

    let mut runs = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let mut bytes_out = 0usize;
        for (key, tx_location) in &entries {
            let entry_bytes = StoredEntryFixed::new(key, *tx_location)
                .to_bytes()
                .expect("encode StoredEntryFixed<TxLocation>");
            bytes_out += entry_bytes.len();
        }
        std::hint::black_box(bytes_out);
        runs.push(started.elapsed());
    }

    report(
        "StoredEntryFixed<TxLocation> encode",
        ENTRIES,
        "entries",
        &mut runs,
    );
}

/// Per-entry decode + checksum-verify cost — the read-path twin of
/// [`stored_entry_fixed_encode`]. This is what a checksum-verifying read-back
/// (metadata load, migrations) pays per entry; sensitive to `verify`
/// re-serializing the decoded item per candidate version instead of hashing
/// the stored bytes.
#[test]
#[ignore = "benchmark: run with `cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks`"]
fn stored_entry_fixed_decode_verify() {
    const ENTRIES: usize = 100_000;
    const RUNS: usize = 5;

    let encoded: Vec<([u8; 32], Vec<u8>)> = (0..ENTRIES as u64)
        .map(|i| {
            let key = synthetic_txid(i);
            let tx_location = TxLocation::new((i / 1_000) as u32, (i % 1_000) as u16);
            let bytes = StoredEntryFixed::new(key, tx_location)
                .to_bytes()
                .expect("encode StoredEntryFixed<TxLocation>");
            (key, bytes)
        })
        .collect();

    let mut runs = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        for (key, bytes) in &encoded {
            let entry = StoredEntryFixed::<TxLocation>::from_bytes(bytes)
                .expect("decode StoredEntryFixed<TxLocation>");
            assert!(entry.verify(key), "checksum verify failed");
        }
        runs.push(started.elapsed());
    }

    report(
        "StoredEntryFixed<TxLocation> decode+verify",
        ENTRIES,
        "entries",
        &mut runs,
    );
}

/// Encode cost of a large `StoredEntryVar` value — stands in for the
/// per-block list entries (`TxidList`, transparent / sapling / orchard
/// lists), where `new` serializes the whole list for the checksum and
/// `to_bytes` serializes it all over again.
#[test]
#[ignore = "benchmark: run with `cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks`"]
fn stored_entry_var_encode() {
    const TXIDS_PER_LIST: usize = 2_000;
    const LISTS_PER_RUN: usize = 50;
    const RUNS: usize = 5;

    let txids: Vec<TransactionHash> = (0..TXIDS_PER_LIST as u64)
        .map(|i| TransactionHash::from(synthetic_txid(i)))
        .collect();
    let list = TxidList::new(txids);
    let key_bytes = 1u32.to_be_bytes();

    let mut runs = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let mut bytes_out = 0usize;
        for _ in 0..LISTS_PER_RUN {
            let entry_bytes = StoredEntryVar::new(key_bytes, list.clone())
                .to_bytes()
                .expect("encode StoredEntryVar<TxidList>");
            bytes_out += entry_bytes.len();
        }
        std::hint::black_box(bytes_out);
        runs.push(started.elapsed());
    }

    println!("[bench] list shape: {TXIDS_PER_LIST} txids per list, {LISTS_PER_RUN} lists per run");
    report(
        "StoredEntryVar<TxidList> encode",
        LISTS_PER_RUN,
        "lists",
        &mut runs,
    );
}
