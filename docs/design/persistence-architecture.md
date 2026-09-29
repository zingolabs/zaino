# Persistence architecture

How Zaino stores what it indexes, with the decisions measured rather than
asserted. [index-data-structures.md](./index-data-structures.md) covers *shape*,
what each fold retains; this covers the substrate the shapes sit on.

**[measured]** = run on the machine in §1. **[estimated]** = arithmetic over
measured inputs. **[assumed]** = taken from upstream or another document, not
independently checked.

The substrate is append-only files plus read-only mmap (Shape A) and immutable
sorted segments (Shape B, `zaino_persistence::lsm`). Nothing is rewritten in place,
nothing is mapped writable, and there is no embedded key-value engine. This
document records why, and the hazards that come with mmap.

## 1. Method

|            |                                                      |
| ---------- | ---------------------------------------------------- |
| CPU        | 13th Gen Intel Core i9-13900H, 14 cores / 20 threads |
| RAM        | 93 GiB                                               |
| Disk       | NVMe (`/dev/nvme1n1p2`)                              |
| Filesystem | **btrfs, `compress=zstd:3`, `ssd`, `discard=async`** |
| Kernel     | 7.1.6                                                |
| rustc      | 1.96.0, `--release` (`opt-level = 3`)                |

**The filesystem is a confound and is treated as one.** btrfs is copy-on-write
with transparent zstd compression, so every cold random 4 KiB read decompresses
an entire extent. Cold-path absolutes are therefore pessimistic against anything
that writes randomly; warm numbers, space figures and relative CPU comparisons
are unaffected.

Cold measurements drop the page cache with `posix_fadvise(POSIX_FADV_DONTNEED)`
**from a separate process that holds no mapping**: `DONTNEED` cannot evict
pages mapped into the calling process, so an in-process "cold" run measures
nothing (6.3 µs against the real 296 µs for a cold tree-state read).

## 2. The workload, in numbers

| Index               | Records                                                 | Bytes each               | Total                                      | Read shape                       | Read rate                   |
| ------------------- | ------------------------------------------------------- | ------------------------ | ------------------------------------------ | -------------------------------- | --------------------------- |
| compact block       | ~3.4M                                                   | ~1–10 KiB                | ~50–100 GiB [assumed]                      | contiguous height span, streamed | 1 per `GetBlockRange`       |
| tree state          | ~4.5M nodes/pool                                        | 32                       | **137 MiB/pool**, 412 MiB for 3 [measured] | ≤33 scattered 32 B reads         | **~34,000 per wallet sync** |
| transparent address | ~190M rows per set (Blockchair outputs count, Sep 2026) | 69 B receive, 72 B spent | ~27 GiB                                    | prefix range scan + point get    | tens per session            |

Two facts dominate everything else:

- **The tree-state working set fits in RAM.** 412 MiB across three pools. Any
  server with 1 GiB to spare serves every `GetTreeState` from page cache after
  the first pass; the cold path matters once, at start-up.
- **The write side is not a constraint.** Sync is I/O bound on validator RPC at
  2.67% CPU. Write throughput is therefore not a ranking factor. Write
  *amplification* still is, because it becomes space.

## 3. `H::combine`, measured

The tree-state layout rests on how expensive one commitment-tree node hash is.
Benchmark: `--release`, `std::time::Instant`, best of 5 runs of 2,000–200,000
iterations with a warm-up pass, `black_box` on inputs and results, against the
workspace's patched `orchard` and `sapling-crypto`.

| Operation                                                     | µs/call [measured] |
| ------------------------------------------------------------- | ------------------ |
| `MerkleHashOrchard::combine` — **as shipped**                 | **100.2**          |
| `MerkleHashOrchard::combine` — `HashDomain` hoisted           | **83.1**           |
| `HashDomain::new(MERKLE_CRH_PERSONALIZATION)` alone           | **17.3**           |
| `HashDomain::hash_to_point` (Sinsemilla loop, no `extract_p`) | 76.8               |
| `pallas::Base::invert`                                        | 6.5                |
| `sapling_crypto::Node::combine`                               | **38.7**           |

`-C target-cpu=native` changed nothing material. Level is irrelevant —
`combine` hashes a 10-bit level prefix into a fixed 520-bit message, so cost
does not vary with tree height.

### 3.1 Why nothing is replayed on read

At 100 µs per combine, any scheme that reconstructs a historical frontier by
replaying commitments is dead on arrival:

| Scheme                         | Storage, 3 pools       | Read cost per `GetTreeState`             |
| ------------------------------ | ---------------------- | ---------------------------------------- |
| Frontier per height            | 4.6 GB [assumed]       | 1 read                                   |
| Checkpoint every 1000 + replay | 4.6 MB                 | ~1500 combines = **~150 ms** [estimated] |
| Checkpoint every 16 + replay   | 290 MB                 | ~24 combines = **~2.4 ms** [estimated]   |
| **Retained-node store**        | **412 MiB** [measured] | **3.0 µs warm / 296 µs cold** [measured] |

At 34,000 requests per wallet sync, `batch = 1000` replay is **85 minutes of
server CPU per wallet**. Even `batch = 16` — already 70% of the node store's
space — is 82 seconds per wallet and 800× slower per request. Retaining the
nodes is not a compromise; it is free.

### 3.2 The per-call `HashDomain` is real, and smaller than it looks

`orchard/src/tree.rs` constructs `HashDomain::new(MERKLE_CRH_PERSONALIZATION)`
inside `combine`, and sinsemilla's `new` is a full group hash of a compile-time
constant, on every node hash. Hoisting it into a `OnceLock` saves **17.3 µs of
100.2, or 17%** \[measured\]: a real upstream fix, not a multiple. The cost is the
Sinsemilla loop itself: 52 ten-bit windows × (one incomplete addition + one
doubling) = 76.8 µs of the remaining 83.1.

### 3.3 What a full-chain fold costs

One combine per commitment, amortised. At N ≈ 3M per pool \[assumed\]:

| Pool                       | Node                   | Fold cost [estimated]     |
| -------------------------- | ---------------------- | ------------------------- |
| sapling                    | `sapling_crypto::Node` | 3M × 38.7 µs = **116 s**  |
| orchard                    | `MerkleHashOrchard`    | 3M × 100.2 µs = **301 s** |
| ironwood                   | `MerkleHashOrchard`    | 3M × 100.2 µs = **301 s** |
| **total, single-threaded** |                        | **~12 minutes of CPU**    |

It is the one place in Zaino where CPU is the bottleneck. Sandblast
(mainnet ~1.70M–1.72M) makes that real: blocks carry hundreds of Sapling
outputs each, and a single-threaded fold held zainod to ~100 blocks/s on one
pegged core, 90% of its CPU in `PoolFold` [measured, profile of a live sync].
So:

1. **The fold is the only fold, and it is level-synchronous.** A batch of leaves
   hashes one tree level at a time; each level's pairs split across every core
   (rayon, under `zaino_sync::compute`). Per-block `apply` is a batch of one
   block, so there is no second fold to agree with. The emitted node set is a
   pure function of (start size, leaves), held by a property test against a
   naive tree for arbitrary splits into batches.
1. **The three pools share nothing**, so they fold concurrently.
1. **The Orchard MerkleCRH domain is derived once** (the 17% above, applied in
   the orchard fork).

## 4. Direct addressing beats a keyed store, on every axis [measured]

Dataset: N = 3,000,000 commitments, one pool, 4,500,021 stored nodes (level 0
all indices, levels 1–31 even indices only). Request shape: 32 scattered 32-byte
reads at a random position, 2,000 requests warm / 300 cold.

| Layout                                       | Size          | Warm, first touch | Warm, resident | Cold       |
| -------------------------------------------- | ------------- | ----------------- | -------------- | ---------- |
| **32 fixed-stride files + mmap**             | **137.3 MiB** | **3.02 µs**       | 0.17 µs        | **296 µs** |
| LMDB via `heed`, key `[level u8][idx u32be]` | 210.2 MiB     | 12.71 µs          | 7.97 µs        | 432 µs     |
| `redb` 4.3, same key                         | 257.0 MiB     | 14.61 µs          | —              | 796 µs     |

48 bytes per commitment × 3M = 137.3 MiB, to the byte. Direct addressing is 1.5×
smaller than LMDB and 1.9× smaller than redb, 4× faster warm and 1.5× faster
cold — no trade is being made. The reason is structural: the key space is dense,
contiguous and arithmetically derivable, so a B-tree stores ~5 bytes of key and
~8 bytes of node header per 32-byte payload and then makes you walk four levels
of it to find something whose address you already knew.

### 4.1 What actually breaks, and what does not

- **Sparse levels — a non-issue.** At N = 3M, levels 22–31 hold 0 or 1 node
  each; the whole tail is under 4 KiB.
- **File count — measurable, and small.** 32 files per pool × 3 pools, plus
  `heights.idx` and 3 × `subtrees.dat`, is ~100 open files and ~100 mmaps. 100
  VMAs is noise; the per-process limit is 65,530 by default, and the page cache
  is indifferent to how many mappings reference it.
- **Torn writes — solved by the layout.** Node addresses are a pure function of
  position, so a crash between the node fsync and the manifest leaves surplus
  bytes past the seal, dropped at open and rewritten byte-identically. Integrity
  is per 4 KiB page, not per node ([durability.md](./durability.md) §3): 0.1%
  overhead instead of 12.5%.
- **fsync fan-out — the one real cost.** Below.

### 4.2 fsync is linear in file count; remap is free [measured]

50 commits, each appending 4 KiB to every file in the fan and then `fsync`ing
all of them, plus the re-`mmap` of every file that the `ArcSwap` snapshot idiom
performs per commit:

| Files in fan                              | append + fsync all | remap all |
| ----------------------------------------- | ------------------ | --------- |
| 1                                         | 3.3 ms             | 7 µs      |
| 3 (the compact-block store)               | 10.1 ms            | 21 µs     |
| 32 (one pool)                             | 115.1 ms           | 180 µs    |
| 96 (three pools)                          | 337.4 ms           | 200 µs    |
| 100 (+ `heights.idx`, 3 × `subtrees.dat`) | **357.8 ms**       | 235 µs    |

~3.5 ms per file, and the remap worry is a non-issue at 235 µs for 100 mappings.
At `batch = 1000` that is ~20 minutes of fsync across a 3.4M-block sync
[estimated] — absorbable against an RPC-bound sync, but not free.

btrfs inflates the per-file figure: its fsync goes through a log tree and a CoW
metadata update. On ext4 the same call is typically sub-millisecond. **The
linearity is the portable part.**

**So the tree-state writer syncs only the level files that grew since the last
fsync.** Level ℓ receives ~n/2^(ℓ+1) new nodes per n commitments, so the upper
levels are clean in almost every batch.

### 4.3 Append-only files are one extent; rewritten pages are half a million

A copy-on-write B-tree file after 10M random-order inserts: **478,526 extents**
for 2.3 GiB, roughly one extent per 5 KiB — every page landed somewhere new. The
append-only file of the same experiment: **1 extent** \[measured, `filefrag`\].
Removing that fragmentation was worth 2× on cold point reads.

This is the deployment consequence of the storage choice, and it is a
consequence in Zaino's favour: an append-only file gets a single extent on any
filesystem, because it is never rewritten in place. Zaino's data directory needs
no `nodatacow` subvolume, no `chattr +C`, and no filesystem advice at all.

## 5. mmap hazards that actually bite

Zaino maps ~100 files. These are not hypothetical.

### 5.1 Page faults inside async tasks

A range read copies or slices up to a 1 MiB span out of the mapping. That is a
synchronous, unbounded-latency operation on whatever thread polls the stream.
1 MiB span, 40 random offsets, page cache dropped from a separate process before
each cold run \[measured\]:

|                                    | Cold         | Warm      |
| ---------------------------------- | ------------ | --------- |
| `copy_from_slice` off the mmap     | **11.70 ms** | 263 µs    |
| `pread` into a heap buffer         | 19.03 ms     | 173 µs    |
| `madvise(MADV_WILLNEED)` then read | **2.74 ms**  | **92 µs** |

- **`MADV_WILLNEED` is a 4.3× cold win and a 2.9× warm win, for four lines.**
  The kernel issues the readahead asynchronously and the read then faults
  against pages already in flight, instead of one serial fault chain.
- **`pread` is not the fix.** It is slower than the naive mmap cold path: one
  synchronous request with no overlap, unable to reuse pages the mapping has.
- **11.7 ms on a runtime worker is a bug.** Tokio's budget for a non-yielding
  section is tens of microseconds. The refill goes through `spawn_blocking` —
  one hop per 1 MiB window, amortised over ~100–1000 records — *and* issues the
  advice.

Absolutes here were taken on an otherwise idle disk; a run under contention
showed 6.7 / 2.4 / 0.29 ms for the same three. Treat the ratio as the finding.

### 5.2 SIGBUS on truncation and on ENOSPC

Touching a mapped page past a shrunken EOF raises SIGBUS, which Rust cannot
catch: the process dies with no unwind and no error. Two rules close it:

1. **Truncation happens only in `open()`, before the first publication.**
   Dropping an uncommitted tail obeys this, and nothing reopens an index in a
   running process (zainod exits on failure; no in-process restart).
   `zainod verify` truncates nothing and maps nothing, so it is safe beside a
   running daemon. A `LOCK` per index directory keeps a second zainod from
   truncating under this one's mappings.
1. **Never map writable.** Zaino maps read-only (`Mmap`, never `MmapMut`) and
   writes through the file descriptor, so a full disk surfaces as an
   `io::Error` from `write_all` rather than as a signal. This is the single most
   valuable structural decision in the store.

### 5.3 `msync` vs `fsync` — a non-question, by construction

Because nothing is ever written *through* a mapping, `msync` never enters the
picture. Durability is `File::sync_data()` on the write fd; the mapping is a
read view, replaced after the sync. That reduces the durability story to
ordinary file I/O with one ordering rule: data → fsync → index → fsync.

The residual hazard is the opposite of a stale read: `Mmap::map` captures the
file length at map time, so a mapping can be *longer* than what was fsynced if a
commit was interrupted. Reads are gated on the durable prefix rather than on the
map length. Any new mmap-backed index must copy that field, not just the
mapping.

### 5.4 `MAP_POPULATE` is not used

`MmapOptions::populate()` pre-faults the whole mapping.

- **Compact blocks: never.** `blocks.dat` is tens of GiB; populating it reads
  the whole chain at start-up.
- **Tree state: not needed.** 412 MiB across three pools; the 296 µs cold path
  is paid once per page and the working set then stays resident (§2). Populating
  in a helper shared with publication would re-read 412 MiB *per batch*, since
  each commit remaps.

### 5.5 Transparent hugepages — a non-event

THP does not back file-backed mappings on a stock kernel
(`CONFIG_READ_ONLY_THP_FOR_FS` is opt-in and khugepaged-driven), and none of
Zaino's mappings are anonymous. The correct action here is none.

### 5.6 Zero-copy reads, and what they cost

Serving a window by `Bytes::copy_from_slice` out of the mapping costs 92–263 µs
per MiB warm [measured] plus a 1 MiB allocation, purely so the mapping stays
droppable. `Bytes::from_owner` over an `Arc<Mmap>` deletes both: a served slice
carries its own handle on the mapping and outlives the snapshot it came from.
The compact-block index does this, and it is sound **only** under the §5.2 rule
that mapped files are never truncated after publication: breaking that rule
corrupts served responses rather than merely crashing.

The advice in §5.1 still precedes a window read. Without a copy there is nothing
to fault the pages in, so they would instead fault wherever the socket reads the
slice: on a runtime worker rather than on the blocking step that expects it.
