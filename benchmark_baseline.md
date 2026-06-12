# Write-path benchmark baseline

Baseline numbers for the `zaino-state` finalised-state write path, captured
before applying the optimizations proposed in the PR
[#1207](https://github.com/zingolabs/zaino/pull/1207) review threads
([r3398927655](https://github.com/zingolabs/zaino/pull/1207#discussion_r3398927655),
[r3399020933](https://github.com/zingolabs/zaino/pull/1207#discussion_r3399020933)).

Post-optimization results are appended in
[Results after optimizations](#results-after-optimizations).

## Environment

| | |
|---|---|
| Commit | `3e23f97` (branch `optimize_sync`) |
| Date | 2026-06-11 |
| CPU | 12th Gen Intel Core i7-1280P |
| Kernel | Linux 7.0.11-arch1-1 |
| rustc | 1.95.0 |
| Profile | nextest `test` profile (optimized + debuginfo) |

## How to reproduce

```sh
# Full suite (tempdirs land on /tmp — tmpfs on this machine):
cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture benchmarks

# Disk-backed ingest (point TMPDIR at a real filesystem so the per-block
# durable LMDB commit pays its actual fsync cost):
mkdir -p target/bench-tmp
TMPDIR=$PWD/target/bench-tmp cargo nextest run -p zaino-state \
  --run-ignored ignored-only --no-capture -E 'test(chain_ingest)'
```

Benchmark sources: `packages/zaino-state/src/chain_index/tests/benchmarks.rs`.

## End-to-end `write_block` ingest

Regtest vector chain: **201 blocks, 830 transactions**; fresh `ZainoDB` per
run, 3 runs, median reported.

| Tempdir backing | Median total | Per block | Throughput |
|---|---:|---:|---:|
| tmpfs (`/tmp`) | 13.98 ms | 69.6 µs | 14,373 blocks/s |
| ext4 (`TMPDIR=target/bench-tmp`) | 167.07 ms | 831 µs | 1,203 blocks/s |

## Serialization micro-benchmarks

CPU-only; independent of disk backing. 5 runs, median reported.

| Benchmark | Work per run | Per item | Throughput |
|---|---|---:|---:|
| `StoredEntryFixed<TxLocation>` encode | 100,000 entries | 254 ns | 3.94 M entries/s |
| `StoredEntryFixed<TxLocation>` decode+verify | 100,000 entries | 246 ns | 4.06 M entries/s |
| `StoredEntryVar<TxidList>` encode (2,000 txids/list) | 50 lists | 92.4 µs | 10,819 lists/s |

## Interpretation

- The ext4 ingest is **12× slower** than tmpfs for identical work: on real
  disk, ~760 µs of the 831 µs per block is the durable LMDB commit (two
  fsyncs) plus the disk-touching post-commit validation read-back. This
  matches the r3399020933 analysis that the I/O side is where slow sync
  lives.
- The CPU-side costs targeted by the single-pass loop refactor
  (r3398927655) and the `entry.rs` serialization fixes (encode-once,
  streaming checksum input, verify-without-re-encode) all live inside the
  ~70 µs/block tmpfs slice. Gauge those changes against:
  - the **micro-benchmarks** for the `entry.rs` fixes (expect roughly 2× on
    encode, more on verify), and
  - the **tmpfs ingest** number for `write_block`-internal changes
    (single-pass loop, encoding outside the write txn, dropping per-tx
    bundle clones).
- Moving the **ext4 ingest** number materially would require amortizing
  commits across blocks (batched writes) or relaxing durability — a design
  decision beyond the current proposals.

## Results after optimizations

### `e30c5556` — single-pass `write_block` loop (r3398927655)

Folds the reverse-`txid_location` build into the main per-transaction loop
and removes the feature-gated O(n²) in-block prevout scan. Performance is
**neutral within run-to-run noise** on this chain (tmpfs medians 69–87 µs
per block across repeat invocations vs the 69.6 µs baseline): at ~4
transactions per vector block the removed work sits below the noise floor.
The win is structural and scales with per-block transaction count.

Noise characterization from repeat runs: tmpfs ingest medians cluster at
13.9–14.9 ms; ext4 is bimodal across invocations (160–170 ms typical,
occasional ~440 ms runs), so single ext4 runs should not be compared in
isolation.

### `dcbf57c5` — byte-budgeted batched writes

`WriteBatcher` + `DbV1::write_blocks`: contiguous blocks share one durable
LMDB commit, flushing on a 128 MiB byte budget or when a block depends on
uncommitted batch state (it spends an output created by — or a sibling
output of a transaction spent from by — the pending batch). Reproduce with
the `write_block_chain_ingest_batched` benchmark (same commands as above;
the test runs both a 64 KiB and the default 128 MiB budget).

| Backing | Per-block ingest | Batched (64 KiB budget) | Speedup |
|---|---:|---:|---:|
| ext4 | 830 µs/block (1,205 blocks/s) | 468 µs/block (2,139 blocks/s) | **1.8×** |
| tmpfs | 121 µs/block | 134 µs/block | ~0.9× (no fsync to save) |

Key finding: on this vector chain the **dependency rule, not the byte
budget, is the amortization ceiling**. The regtest wallets spend freshly
created outputs almost every other block, so the chain splits into ~2-block
batches (103 batches at a 64 KiB budget; still 98 at 128 MiB), and the 1.8×
comes from roughly halving commit count. Chains with sparse cross-block
transparent spends (e.g. early mainnet) form much larger batches and
amortize proportionally more of the ~760 µs/block commit cost.

Follow-up that would lift the ceiling: make the batch build-phase reads
pending-aware (consult the open batch before the DB in
`resolve_spent_outpoints_for_set_info`, the double-spend pre-check, and
`apply_prior_block_transitions`), so the byte budget becomes the only flush
trigger. Implemented in the next section.

### `51fb3f02` — pending-aware batch reads (dependency ceiling lifted)

A `PendingBatchState` overlay (accumulator after the latest pending block,
every pending transaction with location and transparent data, every pending
spend) is threaded through the batch build phase and consulted before the
committed tables at each read that can touch uncommitted state. The
already-spent pre-check now also catches double spends across batch blocks.
`WriteBatcher` drops its dependency tracking: the byte budget is the only
flush trigger.

Batch counts on the vector chain: 64 KiB budget 103 → **18** batches;
128 MiB budget 98 → **1** batch.

Single invocation, medians (note: this invocation's per-block ext4 run
landed in the slow bimodal mode at 1.78 ms/block; the baseline's typical
mode is 830 µs/block):

| Backing | Per-block | Batched 64 KiB | Batched 128 MiB |
|---|---:|---:|---:|
| ext4 | 1.78 ms/block (563 blocks/s) | 469 µs/block (2,130 blocks/s) | **137.7 µs/block (7,264 blocks/s)** |
| tmpfs | 103 µs/block | 112 µs/block | **46.9 µs/block (21,315 blocks/s)** |

Speedups: ext4 single-batch is 12.9× this invocation's per-block run and
6.0× the baseline's typical 830 µs mode. tmpfs single-batch is ~2.2× —
one transaction setup and one commit also save CPU when fsyncs are nearly
free.

Correctness: the batched-vs-per-block equivalence test ingests the vector
chain — whose blocks spend each other's fresh outputs almost every other
block, now *inside* batches — and reproduces identical tip, `txid_location`
mappings, and txout-set accumulator state.
