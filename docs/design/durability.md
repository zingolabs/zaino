# Durability

How each index gets from one committed state to the next without a crash, a
lost write or a foreign chain leaving it in a state it cannot vouch for. It
covers commit protocols, recovery, disk integrity and failure policy.
[persistence-architecture.md](./persistence-architecture.md) covers the
substrate (files, mmap) and [index-data-structures.md](./index-data-structures.md)
covers what each index retains.

## 1. Rules

1. **One commit point per index.** Each index directory holds a `MANIFEST`.
   The manifest *is* the committed state: how many heights are durable, the
   hash of the last one, and the seal (length + tail checksum, §3) of every file
   the index reads. Bytes the manifest does not claim are uncommitted, whatever
   they contain.
1. **One direction.** Every file is append-only. A commit appends, seals the
   files it grew (`fsync`), then atomically replaces the manifest. Nothing the
   current manifest claims is ever rewritten or deleted. The one exception is a
   merge, which deletes segment files only after a *later* manifest has stopped
   claiming them.
1. **Validate before durable, never after.** What the bytes mean is settled
   while the index is built: cheap asserts on what is about to be written
   (heights contiguous, blocks linked, segments sorted, appends at the end). Once
   sealed, nothing re-proves meaning: not at open, not on read, not after a
   write. Checking the *data itself* against the chain belongs to the fetch
   layer (TODO in `zaino_source::fetch_pool`: re-hash every fetched block
   against the best chain's hashes before any index sees it).
1. **One disk check, the same everywhere.** Once durable, the only question is
   whether the bytes on disk are the bytes that were sealed. Every file of every
   index answers it the same way: page checksums (§3).
1. **Recovery never guesses.** Opening an index truncates every file to its
   sealed length and deletes every file the manifest does not list. A file
   *shorter* than its seal means committed bytes were lost, which is fatal.
1. **Die, don't repair.** An I/O error on a commit path, a checksum mismatch, a
   chain that does not link, or a violated assert ends the process. zainod never
   retries a failed `fsync`, never restarts in-process, and never rewrites
   committed data. The service manager restarts it; the next boot either opens
   the sealed state or refuses to run.

Everything on disk is derived from the chain, so the cost of rule 6 is a
resync, never lost information.

## 2. The manifest

```text
offset  bytes  field
0       8      magic       b"ZAINOMF\0"
8       2      format      u16 LE, the index's layout version
10      1      kind        1 compact-block, 2 tree-state, 3 transparent-address
11      1      network     0 main, 1 test, 2 regtest
12      4      body_len    u32 LE
16      n      body        index-owned layout (§4)
16+n    4      crc         CRC-32 (IEEE) over bytes [0, 16+n)
```

Every body starts with the chain identity of the committed tip (height count
and tip hash), then the seal of every file the index owns.

**Commit** (`zaino_persistence::dir`):

1. write `MANIFEST.next` (created, truncated) and `fsync` it
1. `rename` it over `MANIFEST`
1. `fsync` the directory

`rename` replaces the name atomically, so a reader sees the old manifest or the
new one, never a mixture. A crash before step 3 leaves either manifest, and both
are valid states.

**Open**:

- remove a stale `MANIFEST.next`
- no `MANIFEST`: the index is fresh only if every file it owns is empty or
  absent; otherwise fatal (`Unmanifested`). A fresh index writes its initial
  manifest before its first data write, so "data without a manifest" is never
  produced by a crash
- wrong magic, format, kind or network, a bad CRC or a short body: fatal

Every index directory also holds a `LOCK` file under an exclusive
`File::try_lock` for the life of the process, so two zainods can never open one
directory (the second one's truncation would run under the first one's live
mappings: SIGBUS, §5.2 of the persistence doc).

## 3. The file layer and page checksums

All index I/O goes through `zaino_persistence::fs::Fs`: create, positional
write (`pwrite`, never the implicit cursor), `set_len`, `sync_data`, `rename`,
`sync_dir`, `remove`, `list`, and a read-only map. `RealFs` is `std::fs` +
`memmap2`; `SimFs` keeps a volatile and a durable image of every file and
directory and enumerates every state a power loss could leave (§7).

Every index file is a `zaino_persistence::pages::PagedFile`:

```text
<file>       append-only data
<file>.crc   CRC-32 LE per complete 4 KiB page of <file>
MANIFEST     per file: Sealed { len, tail }   tail = CRC-32 of the bytes after the last full page
```

- A complete page never changes, so its CRC lives beside it. The tail page still
  grows, so its CRC rides the manifest: a crash can leave a newer `.crc` or a
  longer file, never a committed page with a stale checksum.
- **Seal** = `fsync` the data, write the CRCs of pages completed since the last
  seal, `fsync` the `.crc`. Every file is sealed before the manifest that
  carries its seal.
- **Open** = each file at least as long as its seal (then truncated to it), the
  `.crc` at least as long as its full pages, the tail page read back and
  checked. That is all: at most 4 KiB read per file.
- **Read** = every page checked the first time a read touches it (a per-file
  bitset; checked pages stay checked across commits). A mismatch panics with the
  file and page, and zainod aborts (§6).
- **Offline** = `zainod verify` reads every committed byte of every sealed file,
  sequentially, against the same checksums (`pages::scrub`).

## 4. Per index

### Compact block

Files: `blocks.dat` (framed records), `offsets.idx` (8 B per height: where its
record ends).

Body: count, tip, tree sizes at the tip, the two seals.

Commit: records and offsets appended as blocks arrive → both files sealed →
manifest.

### Block hash

Files: `by_hash/` segments (hash → height). No height → hash file: heights are
every index's native key.

Body: count, tip, the `by_hash` segment list (total rows = count, checked at open).

Commit: one segment for the batch (+ finished merges, below) → manifest. This is
its own commit point, so a by-hash answer is confirmed by the index that serves it.

### Tree state

Files: `heights.idx` (48 B per height: hash, time, three tree sizes),
`<pool>/l00.dat`..`l31.dat` (retained nodes), `<pool>/subtrees.dat` (36 B per
completed subtree: root, completing height).

Body: count, tip, the `heights.idx` seal, and per pool the 32 level seals and
the subtree seal.

Commit: height records, nodes and subtree entries appended (each append asserted
at its file's end) → every grown file sealed → manifest. The durable carry for
the next batch is seeded in memory from the chunk it just wrote.

### Transparent address and the `by_hash` locator: segments

Files: `<set>/<id>.seg` + `.crc`, immutable sorted segments. A segment = records, then
one fence key per ~4 KiB block, then (probed sets) a binary fuse filter.

Body: count, tip, then per set a list of `(id, records, seal)`.

Commit: each new segment written and sealed → segment directories `fsync`ed → manifest
listing the old segments plus the new ones.

Merge (size-tiered, `FANOUT` segments of one tier, on a background thread): the
merged segment is written by streaming its inputs through their page checksums (a
corrupt input dies, never propagates), sealed, and its directory `fsync`ed. It
is not committed on its own. The next batch's manifest lists it in place of its
inputs, and the inputs are unlinked only once that manifest is durable. A crash
at any point leaves either the old list (the merged segment is unlisted and the next
open deletes it) or the new one (the inputs are unlisted and the next open
deletes them).

## 5. Chain identity

- **Network**: in the manifest header. Opening a mainnet index with a testnet
  config is refused.
- **Tip hash**: in the manifest body. `IndexWriter::finalized_tip()` exposes
  it, and the follower checks that every delivered block links to the one
  before it, starting with the stored tip. A block that does not link is
  `FollowError::Unlinked`, which is fatal: a reorg deeper than the window, a
  validator reset or resynced onto another chain, or a directory reused across
  chains can never be spliced onto the old durable prefix.
- **Reorg bound**: the finalised depth is Zebra's `MAX_BLOCK_REORG_HEIGHT`
  (1000) on main and test. Regtest may configure a smaller depth.

## 6. Failure policy

- zainod installs a panic hook that aborts the process after the default hook
  prints. A panic in any task, including a failed assert or a page checksum
  mismatch, ends the process rather than unwinding into something that keeps
  serving.
- Any task ending ends the process with an error. There is no in-process
  restart: re-opening under live mappings breaks the SIGBUS rule, and an
  `fsync` error must never be retried (after a failed writeback the kernel
  marks pages clean, and a retried `fsync` can report success for data that
  never reached the disk; see <https://wiki.postgresql.org/wiki/Fsync_Errors>).

## 7. Testing

- **Crash-point enumeration** (`SimFs`): each store runs a workload, and every
  distinct crash state around every persistence point is reopened. The
  recovered state must be at least the last acknowledged commit and at most the
  last attempted one, must match the model's content, and must accept the next
  commit. Both directions: every row of a recovered commit present, every row of
  a later one absent (LevelDB `fault_injection_test`).
- **Failed I/O** (`SimFs::fail_from`, Pebble `errorfs`): the LSM store workload
  reruns with the 0th, 1st, 2nd, … mutating call failing until it succeeds.
  Each failure must surface as the injected `Err` (never a panic, never
  swallowed; a background merge's at the next commit), the store must refuse
  every later commit, and a restart must recover an acknowledged or the
  attempted commit.
- **Planted bugs**: each LSM invariant check is fed the bug it guards against
  (duplicate key, a key in two segments, a non-advancing extent) and must fire;
  a check never seen firing is not known to work (RocksDB).
- **Model-based** (proptest over generated operation sequences): apply,
  finalize, reset and reopen against a naive model per index: summed tree sizes
  (compact blocks), `incrementalmerkletree` frontiers (tree state), a recomputed
  UTXO set (transparent), a `BTreeMap` per segment set (the LSM store, at fanout
  2, 3 and 8: commits, settled merges, mid-merge reopens, power loss mid-merge,
  scans and probes at random bounds, pinned views re-checked after every later
  step; swarm testing switches step kinds off per case). Sync is driven through a `MockChain` whose best chain
  moves at random, with reorgs up to the window depth.
- **Live** (`zaino_index_construction`, mainnet): every 5 s, `GetTreeState` at
  the tree-state index's durable tip against zebra's `z_gettreestate`, byte for
  byte; at completion, `zainod verify` over every sealed file.
- **TODO**: `cargo-fuzz` targets for the decoders, and a nightly run of the
  store scenarios on a [LazyFS](https://github.com/dsrhaslab/lazyfs) mount to
  check `SimFs` against real syscalls.

References: ALICE, "All File Systems Are Not Created Equal" (OSDI '14);
CrashMonkey (OSDI '18); LevelDB/RocksDB `MANIFEST` + `CURRENT`; PostgreSQL
fsync errors; ZFS/btrfs per-block checksums.
