# Persistence architecture

Zaino stores its indexes as append-only files read through read-only mmap, plus immutable sorted
segments (`zaino_persistence::lsm`) for the indexes keyed by hash or address. Nothing is rewritten
in place, nothing is mapped writable, and there is no embedded key-value engine. This document
records the measurements behind that choice and the mmap hazards that come with it.
[index-data-structures.md](./index-data-structures.md) covers what each index retains, and
[durability.md](./durability.md) covers how it is committed and recovered.

**[measured]** means run on the machine in §1, **[estimated]** means arithmetic over measured
inputs, and **[assumed]** means taken from upstream or another document without an independent
check.

## 1. Method

|            |                                                      |
| ---------- | ---------------------------------------------------- |
| CPU        | 13th Gen Intel Core i9-13900H, 14 cores / 20 threads |
| RAM        | 93 GiB                                               |
| Disk       | NVMe (`/dev/nvme1n1p2`)                              |
| Filesystem | **btrfs, `compress=zstd:3`, `ssd`, `discard=async`** |
| Kernel     | 7.1.6                                                |
| rustc      | 1.96.0, `--release` (`opt-level = 3`)                |

The filesystem is a confound, and we treat it as one. btrfs is copy-on-write with transparent zstd
compression, so every cold random 4 KiB read decompresses a whole extent. Cold absolutes are
therefore pessimistic for anything that writes randomly. Warm numbers, space figures and relative
CPU comparisons are unaffected.

Cold measurements drop the page cache with `posix_fadvise(POSIX_FADV_DONTNEED)` from a separate
process that holds no mapping. `DONTNEED` cannot evict pages mapped into the calling process, so an
in-process "cold" run measures nothing: 6.3 µs, against the real 296 µs for a cold tree-state read.

## 2. The workload, in numbers

| Index               | Records                                                 | Bytes each               | Total                                      | Read shape                       | Read rate                   |
| ------------------- | ------------------------------------------------------- | ------------------------ | ------------------------------------------ | -------------------------------- | --------------------------- |
| compact block       | ~3.4M                                                   | ~1–10 KiB                | ~50–100 GiB [assumed]                      | contiguous height span, streamed | 1 per `GetBlockRange`       |
| tree state          | ~4.5M nodes/pool                                        | 32                       | **137 MiB/pool**, 412 MiB for 3 [measured] | ≤33 scattered 32 B reads         | **~34,000 per wallet sync** |
| transparent address | ~190M rows per set (Blockchair outputs count, Sep 2026) | 69 B receive, 72 B spent | ~27 GiB                                    | prefix range scan + point get    | tens per session            |

Two facts shape everything else. First, the tree-state working set fits in RAM: 412 MiB across
three pools, so any server with 1 GiB to spare serves every `GetTreeState` from the page cache
after the first pass, and the cold path matters once, at start-up. Second, the write side is not a
constraint. Sync is I/O bound on validator RPC, at 2.67% CPU, so write throughput is not a ranking
factor. Write amplification still is, because it turns into disk space.

## 3. `H::combine`, measured

The tree-state layout rests on the cost of one commitment-tree node hash. Each figure is the best of 5
runs of 2,000 to 200,000 iterations after a warm-up pass, `--release`, `black_box` on inputs and
results, against the workspace's patched `orchard` and `sapling-crypto`.

| Operation                                                     | µs/call [measured] |
| ------------------------------------------------------------- | ------------------ |
| `MerkleHashOrchard::combine`, as shipped                      | **100.2**          |
| `MerkleHashOrchard::combine`, `HashDomain` hoisted            | **83.1**           |
| `HashDomain::new(MERKLE_CRH_PERSONALIZATION)` alone           | **17.3**           |
| `HashDomain::hash_to_point` (Sinsemilla loop, no `extract_p`) | 76.8               |
| `pallas::Base::invert`                                        | 6.5                |
| `sapling_crypto::Node::combine`                               | **38.7**           |

`-C target-cpu=native` changed nothing material. The level does not matter either: `combine`
hashes a 10-bit level prefix into a fixed 520-bit message.

### 3.1 Why nothing is replayed on read

At 100 µs per combine, any scheme that rebuilds a historical frontier by replaying commitments is
too slow to serve:

| Scheme                         | Storage, 3 pools       | Read cost per `GetTreeState`             |
| ------------------------------ | ---------------------- | ---------------------------------------- |
| Frontier per height            | 4.6 GB [assumed]       | 1 read                                   |
| Checkpoint every 1000 + replay | 4.6 MB                 | ~1500 combines = **~150 ms** [estimated] |
| Checkpoint every 16 + replay   | 290 MB                 | ~24 combines = **~2.4 ms** [estimated]   |
| **Retained-node store**        | **412 MiB** [measured] | **3.0 µs warm / 296 µs cold** [measured] |

At 34,000 requests per wallet sync, replaying from a checkpoint every 1000 commitments costs 85
minutes of server CPU per wallet. A checkpoint every 16, which already takes 70% of the node
store's space, still costs 82 seconds per wallet and is 800 times slower per request. Retaining the
nodes is not a compromise. It is cheaper on every axis.

### 3.2 The per-call `HashDomain` is real, and smaller than it looks

Upstream `orchard/src/tree.rs` constructs `HashDomain::new(MERKLE_CRH_PERSONALIZATION)` inside
`combine`, and Sinsemilla's `new` is a full group hash of a compile-time constant, repeated on every
node hash. Hoisting it into a `OnceLock` saves 17.3 µs of 100.2, or 17% [measured]. That is a real
upstream fix, but not a multiple. The cost is the Sinsemilla loop itself: 52 ten-bit windows, each
one incomplete addition and one doubling, account for 76.8 µs of the remaining 83.1.

### 3.3 What a full-chain fold costs

A fold costs one combine per commitment, amortised. At N ≈ 3M per pool \[assumed\]:

| Pool                       | Node                   | Fold cost [estimated]     |
| -------------------------- | ---------------------- | ------------------------- |
| sapling                    | `sapling_crypto::Node` | 3M × 38.7 µs = **116 s**  |
| orchard                    | `MerkleHashOrchard`    | 3M × 100.2 µs = **301 s** |
| ironwood                   | `MerkleHashOrchard`    | 3M × 100.2 µs = **301 s** |
| **total, single-threaded** |                        | **~12 minutes of CPU**    |

This is the one place in Zaino where CPU is the bottleneck. The Sandblast spam (mainnet heights
~1.70M to 1.72M) makes it real: those blocks carry hundreds of Sapling outputs each, and a
single-threaded fold held zainod to ~100 blocks/s on one pegged core, with 90% of its CPU in
the tree-state fold's Merkle hashing [measured, profile of a live sync]. We therefore fold in three ways that stack:

1. The fold is level-synchronous. A batch of leaves is hashed one tree level at a time through
   `Frontier::append_batch_visiting`, with one `Hashable::combine_pairs` call per level, and the
   node types split a wide level across every core (the patched `incrementalmerkletree`, `orchard`
   and `sapling-crypto` in the workspace `Cargo.toml`). The fold runs on the rayon pool under
   `zaino_sync::compute`. Per-block apply is the same fold over a batch of one block, so there is
   no second implementation to keep in agreement. The emitted node set is a pure function of the
   starting size and the leaves, which a property test holds against a naive tree for arbitrary
   splits into batches.
1. The three pools share nothing, so they fold concurrently (`rayon::join`).
1. The Orchard MerkleCRH domain is derived once, which is the 17% above, applied in the `orchard`
   fork.

## 4. Direct addressing beats a keyed store, on every axis [measured]

The dataset is N = 3,000,000 commitments in one pool, or 4,500,021 stored nodes (every index at
level 0, even indices only at levels 1 to 31). Each request is 32 scattered 32-byte reads at a
random position, with 2,000 requests warm and 300 cold.

| Layout                                       | Size          | Warm, first touch | Warm, resident | Cold       |
| -------------------------------------------- | ------------- | ----------------- | -------------- | ---------- |
| **32 fixed-stride files + mmap**             | **137.3 MiB** | **3.02 µs**       | 0.17 µs        | **296 µs** |
| LMDB via `heed`, key `[level u8][idx u32be]` | 210.2 MiB     | 12.71 µs          | 7.97 µs        | 432 µs     |
| `redb` 4.3, same key                         | 257.0 MiB     | 14.61 µs          | n/a            | 796 µs     |

At 48 bytes per commitment, 3M commitments is 137.3 MiB, exactly. Direct addressing is 1.5 times
smaller than LMDB and 1.9 times smaller than redb, 4 times faster warm and 1.5 times faster cold,
so no trade is being made. The reason is structural. The key space is dense, contiguous and
computable from the position, so a B-tree stores ~5 bytes of key and ~8 bytes of node header per
32-byte payload, and then walks four levels to find something whose address was already known.

### 4.1 What actually breaks, and what does not

Sparse levels are not an issue. At N = 3M, levels 22 to 31 hold zero or one node each, and the
whole tail is under 4 KiB.

File count is measurable and small. The tree-state index has 100 data files (32 levels and a
`subtrees.dat` per pool, plus `heights.idx`), each with a mapped `.crc` companion, so ~200 files
and mappings. That is noise against the default limit of 65,530 mappings per process, and the page
cache does not care how many mappings reference it.

Torn writes are solved by the layout. A node's address is a pure function of its position, so a
crash between the node fsync and the manifest leaves surplus bytes past the seal, which the next
open drops and the next commit rewrites byte for byte. Integrity is checked per 4 KiB page, not per
node ([durability.md](./durability.md) §3), which costs 0.1% of space instead of 12.5%.

The one real cost is fsync fan-out, below.

### 4.2 fsync is linear in file count; remap is free [measured]

Each of 50 commits appends 4 KiB to every file in the fan and fsyncs all of them, then re-mmaps
every file, as each commit does when it republishes its read snapshot through `ArcSwap`:

| Files in fan                              | append + fsync all | remap all |
| ----------------------------------------- | ------------------ | --------- |
| 1                                         | 3.3 ms             | 7 µs      |
| 3                                         | 10.1 ms            | 21 µs     |
| 32 (one pool)                             | 115.1 ms           | 180 µs    |
| 96 (three pools)                          | 337.4 ms           | 200 µs    |
| 100 (+ `heights.idx`, 3 × `subtrees.dat`) | **357.8 ms**       | 235 µs    |

That is ~3.5 ms per file, and remapping 100 files at 235 µs is not a concern. At one commit per
1000 blocks, fsyncing all 100 files would add ~20 minutes across a 3.4M-block sync \[estimated\]:
absorbable against an RPC-bound sync, but not free. The fan measured here is data files only.
Sealing a file also fsyncs its `.crc` whenever a page completed since the last seal, so a grown
file costs up to two fsyncs.

btrfs inflates the per-file figure, because its fsync goes through a log tree and a copy-on-write
metadata update. On ext4 the same call is typically sub-millisecond. The linearity is the portable
part.

So the tree-state writer seals only the level files that grew since the last seal. Level ℓ receives
about n/2^(ℓ+1) new nodes per n commitments, so the upper levels are clean in almost every batch.

### 4.3 Append-only files are one extent; rewritten pages are half a million

A copy-on-write B-tree file after 10M random-order inserts held 478,526 extents for 2.3 GiB,
roughly one extent per 5 KiB, because every page landed somewhere new. The append-only file of the
same experiment held 1 extent \[measured, `filefrag`\]. Removing that fragmentation was worth a
factor of 2 on cold point reads.

An append-only file gets a single extent on any filesystem because it is never rewritten in place,
so Zaino's data directory needs no `nodatacow` subvolume, no `chattr +C`, and no filesystem advice
at all.

## 5. mmap hazards that actually bite

Zaino maps a few hundred files: every data file and its `.crc`, and every committed segment. The
hazards below are not hypothetical.

### 5.1 Page faults inside async tasks

A compact-block range read serves a window of up to 1 MiB out of the mapping. Touching it is a
synchronous operation of unbounded latency on whichever thread does it. For a 1 MiB span at 40
random offsets, with the page cache dropped from a separate process before each cold run
\[measured\]:

|                                    | Cold         | Warm      |
| ---------------------------------- | ------------ | --------- |
| `copy_from_slice` off the mmap     | **11.70 ms** | 263 µs    |
| `pread` into a heap buffer         | 19.03 ms     | 173 µs    |
| `madvise(MADV_WILLNEED)` then read | **2.74 ms**  | **92 µs** |

`MADV_WILLNEED` is a 4.3 times cold win and a 2.9 times warm win, for four lines of code. The
kernel issues the readahead asynchronously, and the read then faults against pages already in
flight instead of walking one serial chain of faults. `pread` is not the fix: it is slower than the
naive mmap path when cold, because it is one synchronous request with no overlap, and it cannot
reuse pages the mapping already holds.

An 11.7 ms fault on a runtime worker is a bug, since Tokio expects a non-yielding section to take
tens of microseconds. So each window that touches disk runs on `spawn_blocking` (one hop per 1 MiB
window, amortised over ~100 to 1000 records) and issues `MADV_WILLNEED` over the window first.

These absolutes were taken on an otherwise idle disk. A run under contention showed 6.7, 2.4 and
0.29 ms for the same three rows, so treat the ratio as the finding.

### 5.2 SIGBUS on truncation and on ENOSPC

Touching a mapped page past a shrunken end of file raises SIGBUS, which Rust cannot catch: the
process dies with no unwind and no error. Two rules close this off.

1. **Truncation happens only in `open()`, before the first publication.** Dropping an uncommitted
   tail follows this rule, and nothing reopens an index inside a running process (zainod exits on
   failure, with no in-process restart). `zainod verify` truncates nothing and maps nothing, so it
   is safe beside a running daemon. A `LOCK` per index directory keeps a second zainod from
   truncating under this one's mappings.
1. **Never map writable.** Zaino maps read-only (`Mmap`, never `MmapMut`) and writes through the
   file descriptor, so a full disk surfaces as an `io::Error` from the write rather than as a
   signal. This is the single most valuable structural decision in the store.

### 5.3 `msync` vs `fsync`, a non-question by construction

Because nothing is ever written through a mapping, `msync` never enters the picture. Durability is
`sync_data` on the write descriptor, and the mapping is a read view replaced after the commit. That
reduces durability to ordinary file I/O with one ordering rule: data and checksums are fsynced
before the manifest that seals them (durability.md §3).

The remaining hazard is the opposite of a stale read. A mapping captures the file length when it
is created, so it can be longer than what was committed if a commit was interrupted. Reads are
therefore bounded by the sealed length, never by the mapping's length. `Pages` does this for every
index by slicing each mapping to its seal, so a new index gets it by reading through `Pages`.

### 5.4 `MAP_POPULATE` is not used

`MAP_POPULATE` pre-faults a whole mapping. For compact blocks we never want it: `blocks.dat` is
tens of GiB, and populating it would read the whole chain at start-up. For tree state it is not
needed: 412 MiB across three pools, the 296 µs cold path is paid once per page, and the working set
then stays resident (§2). Populating in code shared with publication would also re-read 412 MiB per
batch, since every commit remaps.

Each mapping carries readahead advice instead. Segment sets probed by key are mapped
`MADV_RANDOM`, so a point lookup faults one page rather than a readahead window, and merges map
their inputs `MADV_SEQUENTIAL`.

### 5.5 Transparent hugepages, a non-event

THP does not back file-backed mappings on a stock kernel (`CONFIG_READ_ONLY_THP_FOR_FS` is opt-in
and driven by khugepaged), and none of Zaino's mappings are anonymous. The correct action here is
none.

### 5.6 Zero-copy reads, and what they cost

Serving a window with `Bytes::copy_from_slice` out of the mapping costs 92 to 263 µs per MiB warm
[measured], plus a 1 MiB allocation, purely so the mapping stays droppable. `Bytes::from_owner` over
an `Arc<Mmap>` removes both: a served slice carries its own handle on the mapping and outlives the
snapshot it came from. The compact-block index serves this way, and it is sound only under the
§5.2 rule that mapped files are never truncated after publication. Breaking that rule would
corrupt served responses, not merely crash.

The advice in §5.1 still precedes each window read. A page's first read verifies its checksum and
so faults it in on the blocking step, but a verified page is never read there again. Once evicted,
it would fault wherever the socket reads the slice, on a runtime worker.
