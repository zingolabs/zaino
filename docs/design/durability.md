# Durability

How each index moves from one committed state to the next without a crash, a lost write or a
foreign chain leaving it in a state it cannot vouch for.
[persistence-architecture.md](./persistence-architecture.md) covers the substrate (append-only
files, mmap) and [index-data-structures.md](./index-data-structures.md) covers what each index
retains.

## 1. Rules

Everything Zaino stores is derived from the chain, so the worst a failure can cost is a resync,
never lost information. That is what lets us stop and re-prove the state at the next boot instead
of repairing anything in place. Every index follows the same six rules.

1. **One commit point per index.** Each index directory holds a `MANIFEST`, and the manifest is
   the committed state: how many heights are durable, the hash of the last one, and the seal
   (length and tail checksum, §3) of every file the index reads. Bytes the manifest does not claim
   are uncommitted, whatever they contain.
1. **One direction.** Every file is append-only. A commit appends, seals the files it grew, then
   atomically replaces the manifest. Nothing the current manifest claims is ever rewritten or
   deleted, except that a segment merge deletes its inputs once a later manifest stops listing
   them.
1. **Validate before durable, never after.** What the bytes mean is settled while the index is
   built, by cheap asserts on what is about to be written: heights contiguous, blocks linked,
   segment keys strictly ascending, appends landing at the end of their file. Once sealed, nothing
   re-proves meaning. Checking block data against the chain belongs to the fetch layer and is not
   done yet (the TODO in `zaino_source::fetch_pool` re-hashes every fetched block against the best
   chain before any index sees it).
1. **One disk check, the same everywhere.** Once data is durable, the only question left is
   whether the bytes on disk are the bytes that were sealed. Every file of every index answers it
   with page checksums (§3).
1. **Recovery never guesses.** Opening an index truncates every file to its sealed length and
   deletes every segment the manifest does not list. A file shorter than its seal means committed
   bytes were lost, which is fatal.
1. **Die, don't repair.** An I/O error on a commit path, a checksum mismatch, a chain that does not
   link, or a violated assert ends the process. zainod never retries a failed `fsync`, never
   restarts in-process, and never rewrites committed data. The service manager restarts it, and
   the next boot either opens the sealed state or refuses to run.

## 2. The manifest

```text
offset  bytes  field
0       8      magic       b"ZAINOMF\0"
8       2      format      u16 LE, the index's layout version
10      1      kind        1 compact-block, 2 tree-state, 3 transparent-address,
                           4 block-hash, 5 value-balance
11      1      network     0 main, 1 test, 2 regtest
12      4      body_len    u32 LE
16      n      body        index-owned layout (§4)
16+n    4      crc         CRC-32 (IEEE) over bytes 0 inclusive to 16+n exclusive
```

Every body starts with the committed tip, as the block count from genesis and the tip hash,
followed by the seal of every file the index owns.

A commit (`IndexDir::commit`) writes `MANIFEST.next`, fsyncs it, renames it over `MANIFEST`, then
fsyncs the directory. The rename is atomic, so a crash at any point leaves the old manifest or the
new one, and both are valid states.

Open removes a stale `MANIFEST.next`. With no `MANIFEST`, the index is fresh only if every file it
owns is empty or absent, and otherwise open fails with `Unmanifested`. A fresh index commits an
empty manifest before its first data write, so data without a manifest is never the result of a
crash. A wrong magic, format, kind or network, a bad CRC, or a body length that disagrees with
`body_len` is fatal.

Each index directory also holds a `LOCK` file under an exclusive `File::try_lock` for the life of
the process. Two zainods can never open one directory, because the second one's open-time
truncation would run under the first one's live mappings (SIGBUS,
[persistence-architecture.md §5.2](./persistence-architecture.md)).

## 3. The file layer and page checksums

All index I/O goes through the `zaino_persistence::fs::Fs` trait. Writes are positional, so a
partial write can never move where the next one lands, and a new file, rename or removal is durable
only once its parent directory is fsynced (Linux semantics). `RealFs` is `std::fs` plus `memmap2`.
`SimFs` keeps a volatile and a durable image of every file and directory, and enumerates every
state a power loss could leave (§7).

Every index file is written through `zaino_persistence::pages`:

```text
<file>       append-only data
<file>.crc   CRC-32 LE per complete 4 KiB page of <file>
MANIFEST     per file: Sealed { len, tail, sums }
             tail = checksum of the bytes after the last full page
             sums = CRC-32 of <file>.crc through the last full page
```

A complete page never changes, so its checksum lives beside it. The tail page is still growing, so
its checksum rides in the manifest. A crash can leave a longer file or a newer `.crc`, but never a
committed page with a stale checksum.

Each page's checksum covers its page index as well as its bytes, so a page and its checksum moved
together to another position still fail. The `sums` digest goes a level further, in the style of
ZFS and TigerBeetle, where the parent holds the child's checksum: the manifest is itself checksummed
and atomically renamed, it pins every page checksum through `sums`, and each checksum pins its page.
A data page and its checksum that are both stale, for example after a lost write, or a file swapped
in from elsewhere, therefore fail at open or scrub instead of passing as consistent.

Sealing a file fsyncs its data, then writes and fsyncs the checksums of the pages completed since
the last seal. Every file is sealed before the manifest that carries its seal. Appends start
writeback (`sync_file_range`) every 1 MiB so the seal's fsync finds little left dirty (RocksDB's
`bytes_per_sync`).

Open checks each file is at least as long as its seal (then truncated to it), reads the tail page
back against the manifest, and reads the whole `.crc` back against the `sums` digest. The `.crc`
file is 1/1024 of the data, so that is about 30 MB for a 30 GB `blocks.dat`. A read checks each page the first time it touches it
and records it in a per-file bitset that carries across commits. A mismatch panics with the file,
the page and the remedy, and zainod aborts (§6).

`zainod verify` is the offline check. It reads every committed byte of every file each enabled
index's manifest seals, sequentially, against the same checksums (`pages::scrub`), and prints a
JSON report. It maps nothing and takes no lock, so it is safe beside a running daemon. It exits 0
when clean, 1 on corruption, and 2 when it cannot read the configuration or a manifest.

## 4. Per index

### Compact block

`blocks.dat` holds each block's framed wire record in height order, and `offsets.idx` holds 8 bytes
per height recording where that record ends. The body carries the tip, the three tree sizes at the
tip, and the two seals. A commit appends records and offsets, seals both files, then writes the
manifest.

### Tree state

`heights.idx` holds 48 bytes per height (hash, time, three tree sizes). Each pool has
`l00.dat`..`l31.dat` for the retained nodes of each level and `subtrees.dat` with 36 bytes per
completed subtree (root, completing height). The body carries the tip, the `heights.idx` seal, and
per pool the 32 level seals and the subtree seal.

A commit appends height records, nodes and subtree entries, each asserted to land at its file's
end, seals `heights.idx`, the subtree files and only the level files that grew, then writes the
manifest. The frontier the next batch folds onto is carried in memory from the chunk just written,
so nothing is read back.

### Segment stores: block hash, value balance, transparent address

These three are `LsmStore`s over immutable sorted segments, `<set>/<id>.seg` plus `.crc`. A
segment holds its records, one fence key per 4 KiB block, and, for sets probed by key, a binary
fuse filter. The block-hash index has one set, `by_hash`, and its manifest check requires the
total rows to equal the committed count. Value balance has `outputs`, and transparent address has
`receives` and `spent`. The block-hash index is its own commit point, so a hash-to-height answer
is always confirmed by the index that serves it.

The body carries the tip and, per set, the list of `(id, records, seal)`. A commit writes and seals
one segment per set, fsyncs the segment directories, then writes a manifest listing the old
segments plus the new ones.

Merges are size-tiered. When a tier holds `FANOUT` segments (8 by default), a background thread
merges them, one merge per tier at a time so small merges never queue behind a large one. The merge
streams its inputs through their page checksums, so a corrupt input dies instead of propagating,
then seals its output and fsyncs its directory. It does not commit on its own: the next batch's
manifest lists the output in place of the inputs, and the inputs are unlinked only once that
manifest is durable. A crash therefore leaves either the old list, where the output is unlisted and
the next open deletes it, or the new one, where the inputs are. If a tier's merge falls two windows
behind, the next batch waits for it, which bounds how many segments a read searches.

## 5. Chain identity

The manifest header records the network, so opening a mainnet index with a testnet configuration
is refused.

The manifest body records the tip hash, which `IndexWriter::finalized_tip()` exposes to the
follower. The follower checks that every delivered block links to the one before it, starting from
the stored tip (`FollowError::Unlinked`), and that a replayed block landing on the durable tip is
the one committed there (`FollowError::Diverged`). Both are fatal and require a resync: they mean a
reorg deeper than the window, a validator reset or resynced onto another chain, or a directory
reused across chains, none of which can be spliced onto the old durable prefix.

The finalised depth, `fetch.finalised_depth`, defaults to Zebra's `MAX_BLOCK_REORG_HEIGHT` (1000).
Config validation refuses less on mainnet and testnet, since a reorg the validator accepts could
then reach committed blocks. Only regtest may configure a smaller depth.

## 6. Failure policy

zainod installs a panic hook that aborts after the default hook prints, so a panic in any task,
including a failed assert or a page checksum mismatch, ends the process instead of unwinding into
something that keeps serving. Every stage runs in one `JoinSet`, and any task ending before a
shutdown signal ends the process with an error.

There is no in-process restart. Re-opening under live mappings breaks the truncation rule of
[persistence-architecture.md §5.2](./persistence-architecture.md), and an `fsync` error must never
be retried: after a failed writeback the kernel marks the pages clean, and a retried `fsync` can
report success for data that never reached the disk (see
<https://wiki.postgresql.org/wiki/Fsync_Errors>). The LSM store enforces this itself: after a
commit returns an error, any further commit panics, and the only way on is to reopen from the last
durable manifest.

## 7. Testing

- **Crash-point enumeration.** Under `SimFs`, each store runs a workload and every distinct crash
  state around every persistence point is reopened. The recovered state must be at least the last
  acknowledged commit and at most the last attempted one, must match the model, and must accept
  the next commit. Both directions are checked: every row of a recovered commit is present and
  every row of a later one is absent (LevelDB `fault_injection_test`).
- **Failed I/O.** `SimFs::fail_from` returns `EIO` from the nth mutating call on (Pebble
  `errorfs`), and the LSM store workload reruns failing at the 0th, 1st, 2nd call and so on until it
  succeeds. Each failure must surface as the injected error, never a panic and never swallowed (a
  background merge's surfaces at the next commit). The store must then refuse every later commit,
  and a restart must recover an acknowledged commit or the attempted one.
- **Planted bugs.** Each LSM invariant check is fed the bug it guards against (a duplicate key,
  including one surfacing inside a merge, a key in two segments, a tip that does not advance) and
  must fire. A check never seen firing is not known to work (RocksDB).
- **Model-based.** Proptest runs generated operation sequences against a naive model per index:
  summed tree sizes and fees for compact blocks, `incrementalmerkletree` frontiers for tree state,
  a naive UTXO set for transparent addresses, and a `BTreeMap` per segment set for the LSM store.
  The LSM model runs at fanout 2, 3 and 8 through commits, settled merges, reopens and power loss
  mid-merge, random-bound scans and probes, and pinned views re-checked after every later step,
  with swarm testing switching whole step kinds off per case. Sync is driven through a `MockChain`
  whose best chain moves at random, with reorgs up to the window depth.
- **Live.** `zaino_index_construction` builds every index on mainnet and every 5 s compares
  `GetTreeState` at the tree-state index's durable tip with Zebra's `z_gettreestate`, byte for
  byte. At completion it runs `zainod verify` over every sealed file.
- **Not yet done.** `cargo-fuzz` targets for the decoders, and a nightly run of the store scenarios
  on a [LazyFS](https://github.com/dsrhaslab/lazyfs) mount to check `SimFs` against real syscalls.

References: ALICE, "All File Systems Are Not Created Equal" (OSDI '14); CrashMonkey (OSDI '18);
LevelDB/RocksDB `MANIFEST` and `CURRENT`; PostgreSQL fsync errors; ZFS/btrfs per-block checksums.
