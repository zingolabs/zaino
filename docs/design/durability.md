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
1. **One direction.** Every data file is append-only. A commit appends, seals the files it grew,
   then writes the new manifest into the manifest slot the previous commit did not use (§2).
   Nothing the current manifest claims is ever rewritten or deleted, except that a segment merge
   deletes its inputs once a later manifest stops listing them.
1. **Validate before durable, never after.** What the bytes mean is settled while the index is
   built, by cheap asserts on what is about to be written: heights contiguous, blocks linked,
   segment keys strictly ascending, appends landing at the end of their file. Once sealed, nothing
   re-proves meaning. Checking block data against the chain belongs to the fetch layer: the NFS
   checks every fetched block's hash and merkle root against the verified header chain before any
   index sees it.
1. **One disk check, the same everywhere.** Once data is durable, the only question left is
   whether the bytes on disk are the bytes that were sealed. Every file of every index answers it
   with page checksums (§3).
1. **Recovery never guesses.** Opening an index truncates every file to its sealed length (a
   reserve of zeros past it included, §3) and deletes every segment the manifest does not list. A
   file shorter than its seal means committed bytes were lost, which is fatal.
1. **Die, don't repair.** An I/O error on a commit path, a checksum mismatch, a chain that does not
   link, or a violated assert ends the process. zainod never retries a failed `fsync`, never
   restarts in-process, and never rewrites committed data. The service manager restarts it, and
   the next boot either opens the sealed state or refuses to run.

## 2. The manifest

`MANIFEST` is two fixed 64 KiB slots. Each commit is written into one slot:

```text
offset  bytes  field
0       8      magic       b"ZAINOMS\0"
8       2      format      u16 LE, the index's layout version
10      1      kind        1 compact_block, 2 tree_state, 3 transparent_address,
                           4 block_hash, 5 value_balance, 6 header_chain
11      1      network     0 main, 1 test, 2 regtest
12      8      seq         u64 LE, the commit's sequence number (1, 2, 3, ...)
20      4      body_len    u32 LE
24      n      body        the engine's layout (§4), shaped by the index's schema
24+n    4      crc         CRC-32 (IEEE) over bytes 0 inclusive to 24+n exclusive
```

Every body starts with the committed tip, as the block count from genesis and the tip hash,
followed by the seal of every sequence file and the segment list of every map (§4).

A commit (`IndexDir::commit`) writes commit `seq` over slot `seq % 2`, the slot the previous commit
did not use, then fsyncs the file. The other slot always holds the commit before it, so a crash
mid-write leaves one valid slot whatever the torn one holds. The committed state is the valid slot
with the higher `seq`.

The commit rewrites existing blocks in place, below the end of the file. That changes no
metadata, so its `fdatasync` flushes this file alone. A rename-based commit (write a new file,
rename it in, fsync the directory) changes metadata twice. On ext4 and XFS each metadata change
forces a journal commit, and a journal commit first waits for the dirty data of every other file
on the filesystem, so every index's commit would stall behind every other index's writeback.

Open reads both slots:

- A slot of zeros was never written.
- A slot failing its magic, length or CRC check is read as a torn write, a commit a crash
  interrupted before it was acknowledged. The other slot's commit stands.
- A slot that passes its CRC but names another index kind, format or network is fatal, as is a
  file that is not exactly two slots long (an older layout) or two torn slots.

A CRC cannot tell a torn write from bit rot, so a live slot damaged on disk also rolls the index
back one commit, and the next boot refetches those blocks. For a segment store that rollback
usually still fails, because the older manifest can list segments a later commit already
unlinked (`PageError::Lost`).

With no `MANIFEST`, open creates one: two zeroed slots written to `MANIFEST.creating`, fsynced,
renamed in, then the directory fsynced. `MANIFEST` therefore only ever exists whole, and a leftover
`MANIFEST.creating` is a creation a crash interrupted, removed at open. A fresh index is fresh only
if every file it owns is empty or absent, and otherwise open fails with `Unmanifested`. It commits
an empty manifest before its first data write, so data without a committed manifest is never the
result of a crash.

Because a commit no longer fsyncs the directory, a data file's own name must be made durable
before any manifest names it. The file that creates it does that (§3).

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

A `PagedFile` is opened as one of two kinds (`pages::FileKind`):

- `Segment`, an LSM segment: written once, then sealed for good. It grows by exactly what is
  appended, and the segment writer fsyncs its directory before the segment is listed.
- `Log`, a sequence table's files: appended to commit after commit. When
  open creates the file (or its `.crc`), it fsyncs the parent directory, so the name is durable
  before any manifest names it. The file then grows into a **reserve**: room past its end,
  written with zeros and fsynced, each step the file's size again (64 KiB to 64 MiB). Appends
  overwrite that room in place, so a seal's `fdatasync` finds no size change and no new block to
  allocate, and never waits on the journal (§2). `fallocate` cannot make that room: it leaves
  unwritten extents, and the first write into one is a metadata change again. The cost is
  writing the file's bytes twice, once as zeros. Open truncates the file to its seal, so the
  reserve is dropped at every boot and rebuilt by the next append.

Each page's checksum covers its page index as well as its bytes, so a page and its checksum moved
together to another position still fail. The `sums` digest goes a level further, in the style of
ZFS and TigerBeetle, where the parent holds the child's checksum: each manifest slot is itself
checksummed, it pins every page checksum through `sums`, and each checksum pins its page.
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

`zainod verify` is the offline check. For each enabled index it runs `DiskEngine::verify` with the
index's schema: every committed byte of every file the manifest seals, read sequentially against
the same checksums, into a JSON report. It maps nothing and takes no lock, so it is safe beside a
running daemon; a segment a merge retired between reading the manifest and scrubbing it is
scrubbed again against the newer manifest. It exits 0 when clean, 1 on corruption, and 2 when it
cannot read the configuration or a manifest.

## 4. The engine's tables

Every index is one `DiskEngine` store ([persistence-engine.md](./persistence-engine.md)): its
schema declares sequence and map tables, and one manifest commits all of them.

```text
body = tip ‖ per sequence: Sealed(<name>.dat) [‖ Sealed(<name>.idx) if Variable]
           ‖ per map: count u32 ‖ (id u32 ‖ records u64 ‖ Sealed)*          (schema order)
```

- **Sequence** (compact-block `blocks`, tree-state `heights` and per pool `l00`..`l31` +
  `subtrees`, header-chain `headers`): `<name>.dat` holds records back to back; a `Variable` one
  adds `<name>.idx`, a u64 end offset per record. A commit appends, then seals only the files that
  grew (fsync cost grows with the file count, and tree-state has about 100), before the manifest.
- **Map** (transparent-address `receives` and `spent`, block-hash `by_hash`, value-balance
  `outputs`): a directory of immutable sorted segments, `<map>/<id>.seg` plus `.crc`. A segment
  holds fixed-width rows, one fence key per 4 KiB block and a binary fuse filter over each key's
  first `scope` bytes (the whole key when the scope is 0). A commit writes and seals one segment per
  map with rows, fsyncs the map directories, then writes a manifest listing the old segments plus
  the new ones.

Compact-block keeps no tree sizes in its manifest: the sizes after the tip are its last record's
`ChainMetadata`. The block-hash index is its own commit point, so a hash-to-height answer is always
confirmed by the index that serves it.

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

The manifest body records the tip hash, which every index hands the NFS at boot (its committed
view's tip). The NFS checks every fetched block against the verified header chain, and that each
index's durable block is the verified chain's block at its height (`NfsError::Diverged`). A
divergence is fatal and requires a resync: it means a reorg deeper than the window, a validator
reset or resynced onto another chain, or a directory reused across chains, none of which can be
spliced onto the old durable prefix.

The finalised depth, `sync.finalised_depth`, defaults to Zebra's `MAX_BLOCK_REORG_HEIGHT` (1000).
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
  every row of a later one is absent (LevelDB `fault_injection_test`). Every index, value-balance
  included, has such a test. The manifest's own test runs three commits, so a commit overwrites a
  slot an earlier one used, under every crash state. A slot damaged by hand (the live one, then
  both) pins the rollback-by-one and the refusal of §2.
- **Not measured by `SimFs`.** Whether a sync commits the journal is a property of the real
  filesystem. `SimFs` checks that the reserve and in-place manifest keep every crash state
  correct, not that they save time; that is measured on a live sync.
- **The crash model.** A crash drops, keeps, reorders or tears any write since the last fsync. We
  tear writes in half, on 512-byte sector boundaries (only the first sector, all but the last),
  and with every other sector landing, since a device promises sector atomicity only and may
  persist a write's sectors in any order. We also lose one file's unsynced writes while every
  other file keeps its own, which is the case where the data and the manifest reach the disk but
  the checksums do not. Tests write with a 4 KiB spill threshold, so every enumerated workload
  also runs through the merge scratch files.
- **Failed I/O.** `SimFs::fail_from` returns `EIO` from the nth mutating call on (Pebble
  `errorfs`), and the LSM store workload reruns failing at the 0th, 1st, 2nd call and so on until it
  succeeds. Each failure must surface as the injected error, never a panic and never swallowed (a
  background merge's surfaces at the next commit). The store must then refuse every later commit,
  and a restart must recover an acknowledged commit or the attempted one. `fail_reads_from` does
  the same for positional reads while reopening: each failure is the open's error, and the open
  that succeeds finds every commit. Mapped reads are not injectable in-process; on a real disk
  they fail as `SIGBUS`, which kills zainod, in line with §6.
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
  byte, and `GetBlock` at the compact-block index's durable tip with Zebra's `getblock 2`. At
  completion it runs `zainod verify` over every sealed file.
- **Not yet done.** `cargo-fuzz` targets for the decoders, and a nightly run of the store scenarios
  on a [LazyFS](https://github.com/dsrhaslab/lazyfs) mount to check `SimFs` against real syscalls.

References: ALICE, "All File Systems Are Not Created Equal" (OSDI '14); CrashMonkey (OSDI '18);
LevelDB/RocksDB `MANIFEST` and `CURRENT`; PostgreSQL fsync errors; ZFS/btrfs per-block checksums.
