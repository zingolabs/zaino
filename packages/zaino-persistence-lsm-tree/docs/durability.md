# Durability

How an index directory moves from one committed state to the next without a crash or a lost
write leaving it in a state it cannot vouch for. [persistence-engine.md](./persistence-engine.md)
covers how the backend implements the `zaino-persistence` port, and
[lsm-deletes.md](./lsm-deletes.md) covers removals in map namespaces.

## 1. Rules

Everything Zaino stores is derived from the chain, so the worst a failure can cost is a resync,
never lost information. That is what lets us stop and re-prove the state at the next open instead
of repairing anything in place. Every index directory follows the same six rules.

1. **One commit point per index.** Each index directory holds a `MANIFEST`, and the manifest is
   the committed state: the tip, and the seal (length and tail checksum, §3) of every file the
   index reads. Bytes the manifest does not claim are uncommitted, whatever they contain.
1. **One direction.** Every data file is append-only. A commit appends, seals the files it grew,
   then writes the new manifest into the manifest slot the previous commit did not use (§2).
   Nothing the current manifest claims is ever rewritten or deleted, except that a segment merge
   deletes its inputs once a later manifest stops listing them.
1. **Validate before durable, never after.** What the bytes mean is settled before they are
   written: the port's commit rules (tip strictly increasing, sequences appended at the next
   position, map keys inserted once and removed at most once) are checked against the committed
   state, and the writer checks what it is about to write (segment keys strictly ascending, rows
   of the map's shape, appends landing at the end of their file). Once sealed, nothing re-proves
   meaning.
1. **One disk check, the same everywhere.** Once data is durable, the only question left is
   whether the bytes on disk are the bytes that were sealed. Every file of every index answers it
   with page checksums (§3).
1. **Recovery never guesses.** Opening an index truncates every sequence file to its sealed length
   (a reserve of zeros past it included, §3), restores a torn tail page from its saved copy (§3)
   and deletes every segment the manifest does not list. A file shorter than its seal means
   committed bytes were lost, and the open is refused.
1. **Stop, don't repair.** An I/O error on a write path, a checksum mismatch or a violated check
   is returned as a typed error, and a failed write refuses every later write until the backend
   is reopened (§5). Nothing retries a failed `fsync` and nothing rewrites committed data: the
   next open either opens the sealed state or refuses to.

## 2. The manifest

`MANIFEST` is two fixed 1 MiB slots. Each commit is written into one slot:

```text
field       bytes  meaning
magic       8      b"ZAINOMS4"
format      2      u16 LE, the caller's layout version
name_len    1      u8
name        n      the index name the caller opens it as (at most 255 bytes)
seq         8      u64 LE, the commit's sequence number (1, 2, 3, ...)
body_len    4      u32 LE
body        m      the engine's layout (§4)
crc         4      CRC-32 (IEEE) over every byte of the slot before it
```

Every body starts with the namespaces the directory was committed with, then the committed tip
(the block count from genesis and the tip hash), followed by the seal of every sequence file and
the segment list of every map (§4). An open declaring other namespaces is refused.

A commit (`IndexDir::commit`) writes commit `seq` over slot `seq % 2`, the slot the previous commit
did not use, then fsyncs the file. The other slot always holds the commit before it, so a crash
mid-write leaves one valid slot whatever the torn one holds. The committed state is the valid slot
with the higher `seq`. A body that would not fit its slot fails the write with
`CommitError::ManifestFull`, nothing moved.

The commit rewrites existing blocks in place, below the end of the file. That changes no
metadata, so its `fdatasync` flushes this file alone. A rename-based commit (write a new file,
rename it in, fsync the directory) changes metadata twice. On ext4 and XFS each metadata change
forces a journal commit, and a journal commit first waits for the dirty data of every other file
on the filesystem, so every index's commit would stall behind every other index's writeback.

Open checks the file is exactly two slots long, then reads both bypassing the page cache, so a
write the kernel failed to flush cannot pass as committed. On Linux that is `O_DIRECT`; on tmpfs,
which has no device behind its cache, a plain read; anywhere else a refused direct read refuses
the open. On macOS `F_NOCACHE` is best effort (it does not evict pages already cached).

- A slot of zeros was never written.
- A slot failing its magic, length or CRC check is read as a torn write, a commit a crash
  interrupted before it was acknowledged. The other slot's commit stands.
- A slot that passes its CRC but names another index name or format is refused
  (`OpenError::Mismatch`), as is a file that is not exactly two slots long or two torn slots.

A CRC cannot tell a torn write from bit rot, so a live slot damaged on disk also rolls the index
back one commit. For a segment store that rollback usually still fails, because the older manifest
can list segments a later commit already unlinked (`PageError::Lost`).

With no `MANIFEST`, open creates one: two zeroed slots written to `MANIFEST.creating`, fsynced,
renamed in, then the directory fsynced. `MANIFEST` therefore only ever exists whole, and a leftover
`MANIFEST.creating` is a creation a crash interrupted, removed at open. A fresh index is fresh only
if every file it owns is empty or absent, and otherwise the open is refused. It commits an empty
manifest before its first data write, so data without a committed manifest is never the result of
a crash. Missing directory levels are created one at a time, each new level's parent fsynced.

Because a commit does not fsync the directory, a data file's own name must be made durable before
any manifest names it. Open fsyncs every directory holding sequence files, once per directory,
on every open; the segment writer fsyncs a map's directory before a segment is listed (§3).

Next to the manifest sits `TAILS`, the doublewrite copy of every sequence file's committed tail
page (§3). It has two slots like the manifest, magic `ZAINOTL1`, and commit `seq` writes slot
`seq % 2` and fsyncs it before its manifest.

Each index directory also holds a `LOCK` file under an exclusive lock for the life of the backend.
Two processes can never open one directory, because the second one's open-time truncation would
run under the first one's live mappings (`SIGBUS`, see the crate's usage guide).

## 3. The file layer and page checksums

All I/O goes through the `fs::Fs` trait. Writes are positional, so a partial write can never move
where the next one lands, and a new file, rename or removal is durable only once its parent
directory is fsynced (Linux semantics). `RealFs` is `std::fs` plus `memmap2`. `SimFs` (feature
`testing`) keeps a volatile and a durable image of every file and directory, and enumerates every
state a power loss could leave (§6).

Every file is written through `pages`:

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

- `Segment`, an LSM segment: written once, sealed (data and `.crc` fsynced), read back through
  its checksums, then made read-only, and the segment writer fsyncs its directory before the
  segment is listed. Open never truncates one, and maps exactly its sealed length.
- `Log`, a sequence namespace's files: appended to commit after commit. Open fsyncs the
  directory holding them every time (§2), so a name a crash left unsynced is durable before any
  manifest names it. The file then grows into a **reserve**: room past its end, written with zeros and
  fsynced, each step the file's size again (64 KiB to 64 MiB). Appends overwrite that room in
  place, so a seal's `fdatasync` finds no size change and no new block to allocate, and never
  waits on the journal (§2). `fallocate` cannot make that room: it leaves unwritten extents, and
  the first write into one is a metadata change again. The cost is writing the file's bytes twice,
  once as zeros. Open truncates the file to its seal, so the reserve is dropped at every open and
  rebuilt by the next append.

Each page's checksum covers its page index as well as its bytes, so a page and its checksum moved
together to another position still fail. The `sums` digest goes a level further, in the style of
ZFS and TigerBeetle, where the parent holds the child's checksum: each manifest slot is itself
checksummed, it pins every page checksum through `sums`, and each checksum pins its page. A data
page and its checksum that are both stale, for example after a lost write, or a file swapped in
from elsewhere, therefore fail at open or scrub instead of passing as consistent.

Sealing a file fsyncs its data, then writes and fsyncs the checksums of the pages completed since
the last seal. Every file is sealed before the manifest that carries its seal. Appends start
writeback (`sync_file_range`) every 1 MiB so the seal's fsync finds little left dirty (RocksDB's
`bytes_per_sync`).

A log file's tail page is rewritten in place by the next commit's appends. A device that tears
inside a sector could damage its committed bytes, so before each manifest the write saves every
grown log file's committed tail page to `TAILS` (§2). Open restores a tail page that fails its
checksum from the slot of the manifest it opens, only for the index's own log files and only
within their last committed page; a file already shorter than its seal is still refused as lost.

Open checks each log file is at least as long as its seal (then truncated to it), reads the tail
page back against the manifest, and reads the sealed `.crc` back against the `sums` digest. The
`.crc` file is 1/1024 of the data. A read checks each page the first time it touches it and
records it in a per-file bitset that carries across commits; a map segment's summary and filter
are checked whole when the segment is mapped. A mismatch is `ReadError::Corrupted` naming the
file and page (§5).

`Backend::verify`, and `verify_dir` offline, scrub every committed byte of every file the starting
manifest seals, read sequentially against the same checksums, into the port's `Verification`. A
file found missing is checked against the manifest once more: one a merge retired meanwhile is
skipped, one still listed is reported lost. Verify therefore covers the files committed when it
started. `verify_dir` takes no lock, so it is safe beside a live backend; a missing `MANIFEST` is
an error, not a clean report.

## 4. The engine's namespaces

Every index directory is one `Engine`: its namespaces are the port's `NamespaceSpec`s, and one
manifest commits all of them.

```text
body = namespaces ‖ tip
       ‖ per namespace, in declaration order:
           sequence: Sealed(<name>.dat) [‖ Sealed(<name>.idx) if Variable]
           map:      count u32 ‖ (id u32 ‖ records u64 ‖ filtered u32 ‖ Sealed)*
```

- **Sequence**: `<name>.dat` holds records back to back; a `Variable` one adds `<name>.idx`, a u64
  end offset per record. A write appends, then seals only the files that grew (fsync cost grows
  with the file count), before the manifest.
- **Map**: a directory of immutable sorted segments, `<map>/<id>.seg` plus `.crc`. A segment holds
  fixed-width rows, one fence key per 4 KiB block and a binary fuse filter over each key's first
  `filtered` bytes (the map's `scope`, or the whole key when the scope is 0). Each segment records
  its own `filtered`, so a changed scope is rebuilt at open (each segment rewritten under the merge
  bandwidth limit) without misreading the old ones. A write writes and seals one segment per map
  with rows (two when the buffer holds a key inserted and removed: the values, then a newer
  tombstone-only segment), fsyncs the map directories, then writes a manifest listing the old
  segments plus the new ones.

Every step of a write that can fail runs before the manifest commit: the segments and sequence
pages are written, sealed and mapped, and `TAILS` saved, first. After the commit only two things
can go wrong, and neither fails it: removing files a merge replaced (retried, §5) and starting the
next merges (a failure is returned by the next write).

Merges are size-tiered. When a tier holds `fanout` segments (`LsmConfig`, 16 by default), a
background thread merges them, one merge per tier at a time so small merges never queue behind a
large one, and at most `merge_slots` doing I/O at once within `merge_mib_per_sec`. The merge
streams its inputs through their page checksums, so a corrupt input stops the merge instead of
propagating, seals its output, reads it back through its checksums and fsyncs its directory;
every byte it reads or writes (spill scratch, fences and filter included) is charged to the same
bandwidth, and a cancelled merge stops between chunks. It does not commit on its own: the next write's manifest lists
the output in place of the inputs, and the inputs are unlinked only once that manifest is durable.
A crash therefore leaves either the old list, where the output is unlisted and the next open
deletes it, or the new one, where the inputs are. If a tier's merge falls two windows behind, the
next write waits for it, which bounds how many segments a read searches.

## 5. Failure policy

Nothing in the crate panics in production code: every failure is a typed error through the port,
and the public entry points catch a panic from a dependency or a caller's scan filter as an error.

- **A failed write refuses later writes.** After a write returns an error, every later commit and
  flush returns `CommitError::Poisoned`, and the only way on is to reopen from the last durable
  manifest. Readers keep reading the last published state. An `fsync` error must never be retried:
  after a failed writeback the kernel marks the pages clean, and a retried `fsync` can report
  success for data that never reached the disk (see
  <https://wiki.postgresql.org/wiki/Fsync_Errors>). The reopen reads the manifest past the page
  cache for the same reason (§2).
- **A failed background merge** is returned by the next write, found before that write touched
  anything: nothing is applied and the merge's partial files are removed. The failure's class
  decides what follows:
  - a corrupt or contract-breaking input can never succeed: `CommitError::Corrupted`, writes
    refused;
  - a failed `fsync` of its output or directory, a failed read-back of its own output, or a bug
    (a self-check, an internal error, a panic): `CommitError::WriteFailed`, writes refused;
  - any other I/O error (a create, write or read, a full disk, a thread that failed to start):
    `CommitError::MergeFailed`, writes stay open and the merge runs again later.

  Retries are budgeted: `LsmConfig::max_merge_failures` (3) transient failures in a row on one
  tier of one map, reset by a merge that lands on that tier (another tier's merges never reset
  it). The next one refuses writes as
  `CommitError::OutOfSpace` when the disk or quota is full, else `WriteFailed`.
- **Corruption found while checking a commit's keys** fails that commit as
  `CommitError::Corrupted` and refuses later writes.
- **A file a merge replaced, or a write's scratch file, that cannot be removed** never fails the
  commit (scratch files are never listed, and the segment that spilled to one is whole). The
  removal is retried in shared rounds, every failing file once per round with pauses of 10, 20,
  40 ms and so on between rounds (`LsmConfig::max_retries` rounds, at most 6), then once at
  every later write; the leftover bytes are a metric, logged as a warning, and as an error past
  `LsmConfig::cleanup_alert_bytes`. At open, such a file is removed or the open is refused.
- **A read that finds corruption** is `ReadError::Corrupted`, and an open that finds lost or
  corrupt data is `OpenError::Corrupted`: stop, verify, resync.

## 6. Testing

- **Crash-point enumeration.** Under `SimFs`, a workload runs and every distinct crash state
  around every persistence point is reopened. The recovered state must be at least the last
  acknowledged commit and at most the last attempted one, must match the model, and must accept
  the next commit. Both directions are checked: every row of a recovered commit is present and
  every row of a later one is absent (LevelDB `fault_injection_test`). The manifest's own test
  runs three commits, so a commit overwrites a slot an earlier one used, under every crash state.
  A slot damaged by hand (the live one, then both) pins the rollback-by-one and the refusal of §2.
- **Not measured by `SimFs`.** Whether a sync commits the journal is a property of the real
  filesystem. `SimFs` checks that the reserve and in-place manifest keep every crash state
  correct, not that they save time.
- **The crash model.** A crash drops, keeps, reorders or tears any write since the last fsync. We
  tear writes in half, on 512-byte sector boundaries (only the first sector, all but the last),
  and with every other sector landing, since a device promises sector atomicity only and may
  persist a write's sectors in any order, and garble the first sector of a write that starts
  mid-sector (a device tearing inside a sector, which `TAILS` repairs). We also lose one file's
  unsynced writes while every other file keeps its own, which is the case where the data and the
  manifest reach the disk but the checksums do not; persist some of a directory's unsynced
  entries and not others; and vary several files at once in a fixed number of seeded samples, so
  every run enumerates the same states. A failed `fsync` drops that file's pending writes, as
  Linux does, and making a file read-only is a crash point. Tests write with a 64-byte spill
  threshold, so every enumerated workload also runs through the merge scratch files.
- **Failed I/O.** `SimFs::fail_from` returns `EIO` from the nth mutating call on (Pebble
  `errorfs`), and the workload reruns failing at the 0th, 1st, 2nd call and so on until it
  succeeds. Each failure must surface as the injected error, never a panic and never swallowed.
  The backend must then refuse every later commit, and a restart must recover an acknowledged
  commit or the attempted one. `fail_reads_from` does the same for positional reads while
  reopening: each failure is the open's error, and the open that succeeds finds every commit.
  `fail_removes`, `fail_creates` and `fail_writes` pin the leftover and failed-merge paths of §5,
  and a panicking metrics recorder pins a panic inside a commit or flush. Mapped reads are
  not injectable in-process; on a real disk they fail as `SIGBUS`.
- **Planted bugs.** Each LSM invariant check is fed the bug it guards against (a duplicate key,
  including one surfacing inside a merge, a key in two segments, a tip that does not advance, a
  corrupt merge input) and must fire. A check never seen firing is not known to work (RocksDB).
- **Model-based.** The port's conformance suite runs generated histories against a `BTreeMap` and
  `Vec` model at fanout 2, 3 and 8 through commits, merges, reopens and power loss mid-merge,
  random-bound chunked scans in both directions (bounds shorter and longer than the key
  included), and pinned readers re-checked after every later step. A backend that writes on its
  own (a small write buffer) must recover exactly a prefix of its commits after a power loss.
- **Not yet done.** `cargo-fuzz` targets for the decoders, and a run of the store scenarios on a
  [LazyFS](https://github.com/dsrhaslab/lazyfs) mount to check `SimFs` against real syscalls.

References: ALICE, "All File Systems Are Not Created Equal" (OSDI '14); CrashMonkey (OSDI '18);
LevelDB/RocksDB `MANIFEST` and `CURRENT`; PostgreSQL fsync errors; ZFS/btrfs per-block checksums.
