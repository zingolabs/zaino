# Initial sync (full chain from genesis)

Initial sync measures the time to download and index the entire Zcash
chain from genesis to chain tip (~3.38M blocks) across different Zaino
configurations.

**Test machine (`gmk`):** AMD Ryzen AI 9 HX 370 (12C/24T), 32 GB RAM,
Arch Linux. (The integrated Radeon 890M GPU reserves ~4 GiB from system
RAM for its VRAM; the OS sees ~28 GB available.) Zebra was also running
on the same host during all measurements.

| Configuration | Wall-clock time |
|---|---|
| Without BlockStore | 24h+ (stopped at 24h mark) |
| BlockStore fetch sync | 3h 45min |
| BlockStore + direct zebra-db read (via zebra crates) | ~20 min |

The direct zebra-db read path avoids the block-fetch RPC overhead by
reading finalized blocks straight from Zebra's on-disk database.

---

# BlockStore Performance

BlockStore is a different implementation of the indexer. Benchmarks
below compare Zaino **with BlockStore** vs **without BlockStore**.

## `zaino-concurrent-test` (1000 connections, 1000 blocks each, range 3M–3.38M)

| Metric | Without BlockStore (avg of 2 runs) | With BlockStore (avg of 2 runs) | Improvement |
|---|---|---|---|
| Success rate | 395/1000 (39.5%) | 1000/1000 (100%) | ✅ 100% success |
| Wall-clock time | ~26.2s | ~2.1s | **12.5× faster** |
| Mean fetch time | ~14.6s | ~0.053s | **275× faster** |
| Aggregate throughput | ~15,200 blocks/s | ~484,600 blocks/s | **32× higher** |
| Mean per-conn throughput | ~68 blocks/s | ~19,500 blocks/s | **287× higher** |

## `zaino-check` (single-thread chain integrity, ~90k blocks)

| Metric | Without BlockStore | With BlockStore (avg of 3 runs) | Improvement |
|---|---|---|---|
| Wall-clock time | ~120s | ~0.93s | **129× faster** |
| Errors | 1 gRPC timeout | 0 errors | ✅ no timeouts |
| Blocks checked | 90,296 / 90,736 (99.5%) | 90,745 / 90,745 (100%) | 100% coverage |

### Key takeaways

1. **Without BlockStore**, the default indexer struggles under concurrency — 60%
   failure rate at 1000 connections, and even a single-threaded check timed out
   mid-stream.

2. **With BlockStore**, per-connection fetch time drops from ~14.6s to ~0.05s,
   throughput increases by over 30×, and wall-clock time drops from ~26s to ~2s.

3. **Reliability** went from 39.5% success to 100% — BlockStore handles the
   concurrent load without failures.

---

# Without BlockStore

```
[hanh@gmk zaino]$ time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-check hhanh00/zaino:latest --start-height 3300000 --server http://localhost:9068
Server:    http://localhost:9068
Chain tip: 3390735
Checking range: 3300000..=3390735 (90736 blocks)

  Stream error: code: 'Deadline expired before operation could complete', message: "Error: get_block_range gRPC request timed out."

══════════════════════════════════════════
  Chain Integrity Check — Summary
══════════════════════════════════════════
  Blocks checked:     90296
  Chain breaks:       0
  Hash length errors: 0
  Total errors:       0

  ✅ Chain is VALID — all 90296 blocks link correctly.

real	2m0.326s
user	0m0.007s
sys	0m0.015s

[hanh@gmk zaino]$ time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-concurrent-test hhanh00/zaino:latest --start 3000000 --end 3380000 --blocks 1000 --connections 1000 http://localhost:9068
Pool: 3000000..3380000 (380001 blocks)
1000 connections × 1000 blocks each (ranges overlap)

Results:
  Connections: 400/600/1000  (success / failed / total)
  Per connection: 1000 blocks (400000 total fetched)
  Chain breaks: 0
  Wall-clock time:        26.07s

  Connect time (s):           min    0.000  mean    0.000  max    0.020
  Fetch time (s):             min    1.060  mean   14.462  max   25.350
  Per-connection total (s):   min    1.060  mean   14.463  max   25.350

  Aggregate throughput: 15342 blocks/s across 1000 connections
  Per-connection throughput: 69 blocks/s (mean)

real	0m26.533s
user	0m0.013s
sys	0m0.007s

[hanh@gmk zaino]$ time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-concurrent-test hhanh00/zaino:latest --start 3000000 --end 3380000 --blocks 1000 --connections 1000 http://localhost:9068
Pool: 3000000..3380000 (380001 blocks)
1000 connections × 1000 blocks each (ranges overlap)

Results:
  Connections: 389/611/1000  (success / failed / total)
  Per connection: 1000 blocks (389000 total fetched)
  Chain breaks: 0
  Wall-clock time:        25.84s

  Connect time (s):           min    0.000  mean    0.000  max    0.030
  Fetch time (s):             min    3.071  mean   14.643  max   25.116
  Per-connection total (s):   min    3.071  mean   14.643  max   25.116

  Aggregate throughput: 15057 blocks/s across 1000 connections
  Per-connection throughput: 68 blocks/s (mean)

real	0m26.280s
user	0m0.006s
sys	0m0.013s
[hanh@gmk zaino]$
```

# With BlockStore

```
[hanh@gmk zaino]$ time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-concurrent-test hhanh00/zaino:latest --start 3000000 --end 3380000 --blocks 1000 --connections 1000 http://localhost:9068
Pool: 3000000..3380000 (380001 blocks)
1000 connections × 1000 blocks each (ranges overlap)

Results:
  Connections: 1000/0/1000  (success / failed / total)
  Per connection: 1000 blocks (1000000 total fetched)
  Chain breaks: 0
  Wall-clock time:        2.07s

  Connect time (s):           min    0.000  mean    0.001  max    0.043
  Fetch time (s):             min    0.005  mean    0.058  max    0.187
  Per-connection total (s):   min    0.006  mean    0.058  max    0.187

  Aggregate throughput: 482188 blocks/s across 1000 connections
  Per-connection throughput: 17166 blocks/s (mean)

real	0m2.479s
user	0m0.013s
sys	0m0.011s

[hanh@gmk zaino]$ time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-concurrent-test hhanh00/zaino:latest --start 3000000 --end 3380000 --blocks 1000 --connections 1000 http://localhost:9068
Pool: 3000000..3380000 (380001 blocks)
1000 connections × 1000 blocks each (ranges overlap)

Results:
  Connections: 1000/0/1000  (success / failed / total)
  Per connection: 1000 blocks (1000000 total fetched)
  Chain breaks: 0
  Wall-clock time:        2.05s

  Connect time (s):           min    0.000  mean    0.000  max    0.004
  Fetch time (s):             min    0.005  mean    0.048  max    0.111
  Per-connection total (s):   min    0.006  mean    0.048  max    0.112

  Aggregate throughput: 487084 blocks/s across 1000 connections
  Per-connection throughput: 20828 blocks/s (mean)

real	0m2.549s
user	0m0.011s
sys	0m0.010s
[hanh@gmk zaino]$

[hanh@gmk zaino]$ time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-check hhanh00/zaino:latest --start-height 3300000 --server http://localhost:9068
Server:    http://localhost:9068
Chain tip: 3390744
Checking range: 3300000..=3390744 (90745 blocks)


══════════════════════════════════════════
  Chain Integrity Check — Summary
══════════════════════════════════════════
  Blocks checked:     90745
  Chain breaks:       0
  Hash length errors: 0
  Total errors:       0

  ✅ Chain is VALID — all 90745 blocks link correctly.

real	0m0.990s
user	0m0.013s
sys	0m0.007s

time docker run --network host -v ./data:/app/data -v .:/app/config --entrypoint zaino-check hhanh00/zaino:latest --start-height 3300000 --server http://localhost:9068
Server:    http://localhost:9068
Chain tip: 3390744
Checking range: 3300000..=3390744 (90745 blocks)


══════════════════════════════════════════
  Chain Integrity Check — Summary
══════════════════════════════════════════
  Blocks checked:     90745
  Chain breaks:       0
  Hash length errors: 0
  Total errors:       0

  ✅ Chain is VALID — all 90745 blocks link correctly.

real	0m0.854s
user	0m0.012s
sys	0m0.008s
```
