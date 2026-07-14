---
date: 2026-07-14
ref: df6f1848
ref_short: df6f184
branch: review-sync
job_name: bench-df6f184-par-merge
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

block_range: 1407..3411407
block_count: 3410000
concurrency: 16
batch_size: 1000

algorithm:
  scheduler: batch-emission
  merge: sequential
  extraction: rayon-parallel
  commit: atomic-batch

total_time_secs: 6996.65
blocks_per_sec: 487.4
db_size_mb: 53666
bytes_per_block: 16502
---

# Full Chain Sync — Batch Emission Scheduler

Second complete chain sync with the scheduler batch-emission optimization.
This run emits ALL available blocks for BlockLocal indexes in a single
scheduler cycle instead of one extraction per cycle. Merge is still
sequential (the parallel merge fix was committed after this deployment).

## Configuration

| Parameter | Value |
|-----------|-------|
| Provisioner | `zebra-readstate` (direct RocksDB read-only) |
| Backend | LMDB, 120 GB map, NVMe (`/data/zaino-bench`) |
| Index set | `current_zaino` (8 indexes) |
| Block range | 1,407 → 3,411,407 (3,410,000 blocks) |
| Concurrency | 16 (provisioner sliding window) |
| Batch size | 1,000 blocks per batch |

## Results

| Metric | Value |
|--------|-------|
| **Total time** | 6,996.65 s (116.6 min) |
| **Overall throughput** | **487.4 blocks/s** |
| **DB size** | 53,666 MB (52.4 GB) |
| **Bytes/block** | 16,502 |

## Throughput at Chain Tip (last 20 batches, heights 3.39M–3.41M)

| Stat | blocks/s |
|------|----------|
| mean | 3,868 |
| p50 | 3,912 |
| p95 | 4,487 |
| min | 2,941 |
| max | 4,667 |

## Per-Index Merge Duration (at tip)

| Index | p50 | p95 | max |
|-------|-----|-----|-----|
| headers | 0.3 ms | 0.4 ms | 0.4 ms |
| txids | 0.3 ms | 0.4 ms | 0.5 ms |
| hash_to_height | 0.2 ms | 0.3 ms | 0.3 ms |
| txid_location | 1.1 ms | 1.2 ms | 1.3 ms |
| transparent_data | 1.6 ms | 1.9 ms | 2.1 ms |
| transparent_spends | 1.1 ms | 2.1 ms | 2.3 ms |
| sapling | 0.8 ms | 1.2 ms | 1.2 ms |
| orchard | 0.9 ms | 1.2 ms | 1.3 ms |

## Comparison with Sequential Baseline

| Metric | Sequential | Batch Emission | Delta |
|--------|-----------|----------------|-------|
| **Total time** | 7,337.66 s | 6,996.65 s | -341 s (-4.6%) |
| **Overall throughput** | 464.7 blk/s | 487.4 blk/s | +22.7 (+4.9%) |
| **Tip throughput (p50)** | 2,977 blk/s | 3,912 blk/s | +935 (+31.4%) |
| **Tip throughput (mean)** | 3,075 blk/s | 3,868 blk/s | +793 (+25.8%) |
| **DB size** | 53,668 MB | 53,666 MB | ~identical |

## Observations

1. **4.9% overall improvement** — modest because the bottleneck in mid-chain
   (heights 500k–2M) is block deserialization and LMDB writes, not scheduler
   overhead. The scheduler fix reduces dispatch round-trips from 1000 to ~10
   per batch, but this only matters when extraction is fast.

2. **25–31% improvement at chain tip** — tip blocks are small so extraction
   is fast, and the scheduler overhead that was removed becomes a larger
   fraction of batch time. This is where the optimization shines.

3. **Merge is still sequential** — the `report_extractions` path was calling
   `merge_persist` per-index in a loop. The parallel merge fix (committed as
   `095591cf`) was not included in this deployment. A follow-up run with both
   optimizations will show the combined effect.

4. **DB size is identical** — expected, same indexes indexing the same chain.

5. **sync_channel span: busy=2064s, idle=4932s** — the engine spent 70% of
   wall time idle, waiting for blocks from the provisioner. This confirms
   the provisioner (ReadState block deserialization) is the primary bottleneck,
   not the engine.
