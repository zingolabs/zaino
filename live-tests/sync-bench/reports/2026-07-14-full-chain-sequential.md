---
date: 2026-07-14
ref: df6f1848
ref_short: df6f184
branch: review-sync
job_name: sync-bench
cluster: golden-mainnet
node: tekau

provisioner: zebra-readstate
backend: lmdb
map_size_gb: 120
storage: nvme

index_set: current_zaino
context: CurrentZainoContext
context_fields:
  - height (BlockHeight)
  - hash (BlockHash)
  - prev_hash (BlockHash)
  - time (BlockTime)
  - bits (CompactDifficulty)
  - txids (Vec<TransactionHash>)
  - spends (Vec<(prev_txid, prev_index, spending_txid)>)
  - txid_locations (Vec<(txid, height, tx_index)>)
  - transparent_txs (Vec<TransparentTxCompact>)
  - sapling_txs (Vec<SaplingTxCompact>)
  - orchard_txs (Vec<OrchardTxCompact>)
indexes:
  - name: headers
    scope: BlockLocal
    composition: Append
    key: BlockHeight (8 bytes LE)
    value: HeaderValue (hash + prev_hash + time + bits; 72 bytes)
    block_context: HeaderCtx (height, hash, prev_hash, time, bits)
  - name: txids
    scope: BlockLocal
    composition: Append
    key: BlockHeight (8 bytes LE)
    value: TxidsValue (Vec<TransactionHash>; 32-byte chunks)
    block_context: TxidsCtx (height, txids)
  - name: hash_to_height
    scope: BlockLocal
    composition: Append
    key: BlockHash (32 bytes)
    value: BlockHeight (8 bytes LE)
    block_context: HashToHeightCtx (hash, height)
  - name: txid_location
    scope: BlockLocal
    composition: Append
    key: TransactionHash (32 bytes)
    value: TxLocation (height + tx_index; 12 bytes LE)
    block_context: TxidLocationCtx (Vec<(txid, height, tx_index)>)
  - name: transparent_data
    scope: BlockLocal
    composition: Append
    key: BlockHeight (8 bytes LE)
    value: TransparentBlockValue (Vec<inputs/outputs per tx>)
    block_context: TransparentDataCtx (height, Vec<TransparentTxCompact>)
  - name: transparent_spends
    scope: BlockLocal
    composition: Append
    key: OutpointKey (prev_txid + prev_index; 36 bytes)
    value: TransactionHash (32 bytes)
    block_context: SpendCtx (Vec<(prev_txid, prev_index, spending_txid)>)
  - name: sapling
    scope: BlockLocal
    composition: Append
    key: BlockHeight (8 bytes LE)
    value: SaplingBlockValue (Vec<nullifiers + outputs per tx>)
    block_context: SaplingCtx (height, Vec<SaplingTxCompact>)
  - name: orchard
    scope: BlockLocal
    composition: Append
    key: BlockHeight (8 bytes LE)
    value: OrchardBlockValue (Vec<actions per tx>)
    block_context: OrchardCtx (height, Vec<OrchardTxCompact>)

block_range: 1372..3411372
block_count: 3410000
concurrency: 16
batch_size: 1000

algorithm:
  scheduler: one-extraction-per-cycle
  merge: sequential
  extraction: rayon-parallel
  commit: atomic-batch

total_time_secs: 7337.66
blocks_per_sec: 464.7
db_size_mb: 53668
bytes_per_block: 16503
---

# Full Chain Sync — Sequential Merge Baseline

First complete chain sync with the new sync engine (8 indexes, `current_zaino` set).
This run uses the **pre-optimization** code: one extraction emitted per scheduler
cycle and sequential merge+persist across indexes.

## Configuration

| Parameter | Value |
|-----------|-------|
| Provisioner | `zebra-readstate` (direct RocksDB read-only) |
| Backend | LMDB, 120 GB map, NVMe (`/data/zaino-bench`) |
| Index set | `current_zaino` (8 indexes) |
| Block range | 1,372 → 3,411,372 (3,410,000 blocks) |
| Concurrency | 16 (provisioner sliding window) |
| Batch size | 1,000 blocks per batch |

## Results

| Metric | Value |
|--------|-------|
| **Total time** | 7,337.66 s (122.3 min) |
| **Overall throughput** | **464.7 blocks/s** |
| **DB size** | 53,668 MB (52.4 GB) |
| **Bytes/block** | 16,503 |

## Throughput at Chain Tip (last 24 batches, heights 3.38M–3.41M)

| Stat | blocks/s |
|------|----------|
| mean | 3,075 |
| p50 | 2,977 |
| p95 | 3,592 |
| min | 2,012 |
| max | 3,757 |

Tip blocks are simpler (13k–25k ops/batch vs heavier middle-chain blocks),
so tip throughput is ~6x the overall average.

## Per-Index Merge Duration (at tip)

| Index | p50 | p95 | max |
|-------|-----|-----|-----|
| headers | 0.3 ms | 0.4 ms | 0.6 ms |
| txids | 0.3 ms | 0.4 ms | 0.4 ms |
| hash_to_height | 0.2 ms | 0.3 ms | 0.4 ms |
| txid_location | 1.2 ms | 1.4 ms | 1.7 ms |
| transparent_data | 1.6 ms | 2.1 ms | 4.5 ms |
| transparent_spends | 1.0 ms | 2.0 ms | 3.5 ms |
| sapling | 1.0 ms | 1.2 ms | 1.2 ms |
| orchard | 1.0 ms | 1.2 ms | 3.1 ms |

## Observations

1. **464.7 blocks/s overall** covers the full range including Sprout→Sapling
   activation and shielded pool growth where blocks are heaviest.
2. **Sequential merge** means each batch waits for all 8 indexes to merge
   one after another — wall time is the sum, not the max.
3. **Scheduler emits one extraction per cycle** — 1000 dispatch round-trips
   per batch of 1000 blocks, significant scheduling overhead.
4. The provisioner (ReadState) sustains ~465 blocks/s feeding the channel,
   indicating the engine is the bottleneck, not block fetching.
5. **DB at 52 GB** — reasonable for 8 indexes. ~16.5 KB/block average
   is dominated by transparent_spends and transparent_data.

## Next Steps

- `bench-df6f184-par-merge` running with both optimizations (batch emission
  + parallel merge) for A/B comparison at the same chain heights.
- OTLP tracing to Tempo for flame graph visualization of parallelism.
