# zaino-persistence

What every index stores through: the persistence port (`PersistenceEngine`,
`Store`, `View`, `SequenceRead`, `MapRead`, `Schema`, `BlockChanges`), non-final
data over a committed view (`Overlay`, `OverlayView`: what `zaino-nfs` holds
above the durable root and every snapshot reads through), and the engine behind it, `DiskEngine`, which keeps sequences as positional files and
maps as an LSM, under one manifest. Design: [`docs/design/persistence-engine.md`](../../docs/design/persistence-engine.md);
crash protocol: [`docs/design/durability.md`](../../docs/design/durability.md).

An index owns only its tables (`FORMAT` + `TABLES`, constants) and its record
layouts: fixed-width `encode` / `decode` functions beside a golden-bytes test
(e.g. `zaino-index-tree-state/src/heights.rs`). The engine sees bytes.

## Declaring, opening, committing, reading

```rust
use zaino_persistence::{
    DiskEngine, IndexKind, MapRead, MapTable, PersistenceEngine, Schema, SequenceRead,
    SequenceTable, Store, Tables, View, Width,
};

const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable); // blocks.dat + .idx
const SPENT: MapTable = MapTable::new(0, "spent", Width::fixed(36), Width::fixed(36), 0); // spent/
pub const FORMAT: u16 = 1;
pub const TABLES: Tables = Tables::new(&[BLOCKS], &[SPENT]);   // ids = positions, checked at compile time
pub const WRITE_BUFFER: NonZeroUsize = NonZeroUsize::new(64 << 20).expect("non-zero"); // buffered heap

let schema = Schema::new(IndexKind::CompactBlock, FORMAT, network, TABLES); // Copy, built once
let engine = DiskEngine::new(fs, LsmConfig::default());
let mut store = engine.open(path, &schema, WRITE_BUFFER)?; // fresh = empty, else the committed tip

let mut changes = store.changes(block.at());              // one block's empty delta
changes.sequence(BLOCKS).append(&record);                 // at the end, in call order
changes.map(SPENT).insert(&outpoint.encode(), &spend);    // keys unique
store.apply(changes);                                     // buffered: in staged(), not in view()
                                                          // (WRITE_BUFFER reached = committed here)
store.commit()?;                                          // every buffer, one fsync, then in view()

let view = store.committed();                                  // committed only (what serving pins)
let staged = store.staged();                              // StagedView: committed + buffered (borrow)
view.tip();                                               // Option<BlockRef>
let blocks = view.sequence(BLOCKS);                       // one table of one view
blocks.count(); blocks.record(h); blocks.records(a..b);   // zero-copy mmap slices
let spent = view.map(SPENT);
spent.value(&key); spent.values(&keys);                   // answers in `keys` order
spent.range(&start, &end, limit);                         // [start, end); None = over `limit`
```

- **`IndexKind`** is the manifest's kind tag, and `IndexKind::name()` (`const`,
  snake_case: `compact_block`, …, `header_chain`) is the index's one spelling:
  sink subscription, metric label, statusz key, task name, default directory,
  snapshot entry and a writer's panic messages all read it.
- **Tables** are `const` handles: `id` = position among the schema's sequences
  (or maps), checked by `Tables::new`. A name with a `/` puts the table's files
  in a sub-directory (`"sapling/l00"`). `SequenceId` / `MapId` are the engine
  side of the read traits; an index never names one.
- **Map keys** compare as bytes: encode numeric fields big-endian. They lead
  with at least 8 uniform bytes (a hash, a txid), because each segment's filter
  shards on them. `scope` gives the leading key bytes every range read shares:
  the filter covers that prefix, and a range whose bounds share it visits only
  the segments that may hold it. Scope 0 means point lookups, with whole keys
  filtered and segments mapped for random access.
- **Removals:** a map declaring `MapTable::deletes()` accepts
  `changes.map(T).remove(&key)` beside `insert`. The contract is insert once,
  remove at most once, never re-insert, and per block a key is inserted or
  removed, never both. A removed key reads absent everywhere (`value`, `values`,
  `range`, whose `limit` counts live rows only). `remove` on a map without
  `deletes()` panics, naming it. Such a map's rows carry one flag byte, and
  every other map's layout is unchanged
  ([lsm-deletes.md](../../docs/design/lsm-deletes.md)).
- **`BlockChanges`** holds one buffer per table, shaped by the schema, and is opened
  only by `Store::changes(at)` or `Overlay::changes(at)`. Fixed-width tables cost
  only their bytes; variable ones add an end offset per item. A handle of
  another schema panics at `sequence` / `map` (writes and reads alike), a
  fixed-width item of the wrong size at `append` / `insert`, each naming the
  table.
- **`changes.block()`** = the block a delta is for (its tip once applied). A fold's
  preconditions on it and on the parent linkage are `zaino_sync::assert_next` /
  `assert_run`, not this crate's.
- **`apply`** appends final changes to the store's `WriteBuffer` in RAM: each
  table's items back to back, as `BlockChanges` holds them, map rows unsorted (the
  LSM sorts a batch at commit) with one hash index of row numbers per map.
  `staged()` reads them (`StagedView<'_, V>` = `OverlayView<V, &WriteBuffer>`: a borrow, so
  a writer cannot hold one across `apply`),
  `committed()` and the disk do not until `commit`. It panics, buffering nothing,
  on changes built for another schema, a tip not above the last applied one,
  or a map key the buffer already holds (or one `BlockChanges` inserts twice),
  and on a key removed twice or inserted and removed by one `BlockChanges`. A
  removal of a key the buffer inserted cancels it there: neither row reaches a
  segment.
  `buffered_bytes()` = the buffer's allocated capacity, within ±30% of the real
  heap for every table shape (`tests/buffer_heap.rs` measures it with a counting
  allocator). Once it reaches the `write_buffer` given to `open`, `apply`
  commits by itself; that commit failing panics via `commit_failed`. A budget
  is RAM, not bytes on disk.
- **`commit`:**
  - Every buffered change goes to disk in one commit at the last applied tip:
    appends are sealed (only tables that grew are fsynced) and each map's rows
    are written as one sorted segment, every table on its own thread; then the
    manifest slot is written, which
    is the commit point, and `committed()` moves. Nothing buffered = `Ok`, nothing
    written.
  - An `Err` poisons the store, so any later `commit` panics (an `fsync` error is
    never retried; `durability.md` §6). Drop the store and reopen it: the
    buffer is gone with it.
  - `StoreError::commit_failed(index, path)` turns the `Err` into a panic
    naming the index and directory (`apply`'s own commit and `zaino_sync::commit` call it):
    `<index> index commit failed: disk <dir> full` when `StorageFull` or
    `QuotaExceeded` sits anywhere in the error chain, else
    `<index> index commit failed at <dir>: <error>`.
- **Reads never error.** A page whose checksum fails **panics** on its first
  touch, telling the operator to run `zainod verify`. A `View` never changes
  while held, so a request or stream keeps one state throughout.
- **`open` refuses:**
  - another index kind, format or network (`ManifestError::{Kind, Format, Network}`)
  - data in a directory with no manifest (`Unmanifested`)
  - a file shorter than its seal, a torn tail page, or `.crc` checksums that
    don't match the manifest (`PageError::{Lost, Tail, Sums}`)

  Bytes past the seal (an interrupted commit, or the zeroed reserve) are
  truncated, and segments the manifest doesn't list are removed.
- **Tables the LSM can't hold** panic at `open`, naming the map: a `Variable`
  key or value, a scope longer than the key, or fewer than 8 filtered key bytes.
- **Errors** are `StoreError` (`Io`, `Manifest`, `Page`, `Segment`).

## Verifying offline

```rust
let report = DiskEngine::new(fs, LsmConfig::default()).verify(path, &schema)?;  // plain std::fs reads, no lock
report.heights;          // blocks committed from genesis
report.units;            // Vec<Checked { name, committed_bytes, orphaned_bytes, lost, bad_sums, bad_pages }>
report.is_clean();
```

- Every file the manifest seals is read through and checked page by page.
- Safe beside a running daemon. A segment that a merge retired between reading
  the manifest and scrubbing it is scrubbed again against the newer manifest; a
  file still listed but missing is reported `lost`.

## On disk

```text
<dir>/MANIFEST         two fixed slots; commit n rewrites slot n % 2 in place, then fdatasync
<dir>/<seq>.dat        sequence records back to back      (+ .crc: CRC-32 per 4 KiB page)
<dir>/<seq>.idx        Variable only: u64 LE end offset per record
<dir>/<map>/<id>.seg   one sorted segment per batch or merge
```

- **Manifest:** each slot checks magic, CRC, index kind, format and network. A
  torn slot falls back to the other one. The body is the committed tip, then
  each sequence's seals, then each map's segment list
  (`disk.rs::a_manifest_body_is_its_golden_bytes`).
- **Page checksums:** each file's `.crc` holds a CRC-32 per complete page,
  seeded with the page's index. The tail page's CRC and a digest of the whole
  `.crc` ride the manifest, which binds every page to the commit.
- **Write-ahead reserve:** sequence files grow into a zeroed, fsynced reserve,
  so a seal changes no metadata and never waits on the filesystem journal.
  An append past EOF changes the inode's size (and allocates blocks at writeback),
  so on ext4/XFS its sync commits the journal, which first waits on every other
  file's dirty data. `fallocate` cannot help: its unwritten extents make the
  first write a metadata change again. Each growth step is the file's size again
  (64 KiB to 64 MiB), so zeros written ≈ the data once over; open truncates the
  file to its seal and the next append rebuilds the reserve.
  Writeback starts every 1 MiB appended (`sync_file_range`), so seal-time fsyncs
  find little dirty data.

## LSM behaviour (map tables)

- **Segments:**
  - Packed fixed-width rows (key ‖ value), one fence per ~4 KiB block, an
    in-memory summary over the fences, and a sharded BinaryFuse8 filter
    (FPR 2⁻⁸).
  - A seek is a search in memory, then one page of fences, then one block of
    rows.
  - Each reader reads and checks a segment's whole filter when it maps the
    segment.
- **`values`:**
  - Sorts the keys and resolves them in key order.
  - From 64 keys it prefetches (`MADV_WILLNEED`: every candidate's fence group,
    then its block of records) and resolves the keys on the rayon pool.
  - Below 64 keys it stays on the calling thread, where rayon's wake-up cost
    5–10× the lookups (measured).
- **Merges:** `DiskEngine::new(fs, LsmConfig { fanout, merge_slots })`
  - A tier merges once it holds `fanout` segments (default 16; write amplification
    `log_fanout(rows)`).
  - Run on background threads, at most one per size tier and `merge_slots` doing
    work across every store of one engine (default 4), sharing
    `merge_mib_per_sec` of read + written bytes (default 200; a debt bucket, one
    second of burst; commits are never paced), lowest tier first, at
    background CPU and I/O priority. The I/O priority only binds under a
    scheduler that honours it (`mq-deadline`, `bfq`); `none` ignores it.
  - A finished merge is swapped in by the next commit's manifest, and its
    inputs are unlinked once that manifest is durable.
  - A commit waits for a merging tier only once that tier is two idle windows
    behind (`STALL_WINDOWS`), which bounds read fan-out.
  - Merge errors and panics surface at the next commit.
  - Dropping the store cancels and joins every merge.
- **Page cache:** every segment a commit or merge writes, and every sequence
  append, is dropped from page cache once synced (`POSIX_FADV_DONTNEED`; a map
  keeps only what opening the view reads: summary + filters). A table a fold
  reads declares `cache_writes()` (value-balance's `outputs`, every tree-state
  table) and keeps what it writes cached; the rest fault pages in when served.
  Appends stay as fast (pages are dropped clean, after the fsync); the cost is
  one 4 KiB re-read of a sequence's tail page per commit. Bulk writes and merges
  never evict the tables folds read.
- **Duplicate keys** panic, whether within a batch or across segments (on a
  read or a merge).
- **Tombstones** (`deletes()` maps): a removal written to a segment is a
  tombstone row, in the filter like any key. A point read checks every segment
  whose filter admits the key, and a tombstone in any of them means absent, so
  no answer depends on segment order. A merge that holds a key's value and its
  tombstone drops both, and keeps a lone tombstone. It may write fewer rows
  than it read, or no segment at all. Any other duplicate of a key in a merge
  is `SegmentError::Contract`, surfaced at the next commit.
- **Read-back check:** under `cfg(test)` or feature `testing`, every sealed
  segment is read back: row count, ascending keys, no filter false negative, and
  a CRC of the rows.
- **Metrics:** `zaino_lsm_*`, labelled `set` = the map's name, and per store
  `zaino_store_commit_seconds` / `zaino_store_commit_bytes_total` (`index`).
  Register them with the crate's `describe_metrics`, `METRIC_BUCKETS` and
  `lsm::METRIC_BUCKETS`.
- **`DiskView::footprint()`:** per table, committed bytes and how many of them
  are in page cache (`TableFootprint`; `cached: None` on `SimFs`). One
  `mincore` pass over the mapped files, no I/O.
- **Logs:** `Compacting segments` / `Compacted segments` (debug) and
  `Commit waited on compaction` (warn).

## Non-final data: `Overlay` and `OverlayView`

An `Overlay` is one index's data above a durable tip, as of one block: per
sequence the records past durable's length, per map the rows above durable
(`imbl`, so a clone is O(tables) pointer copies). `zaino-nfs` keeps one per
non-final block. `OverlayView<V, A>` reads any `Uncommitted` over a view: an `Overlay`
(the default) or a store's `WriteBuffer`.

```rust
let root = Overlay::empty(store.committed().schema()); // the committed view's schema
let mut changes = root.changes(block.at());      // the child block's empty delta
fold(&parent, &block, &mut changes);
let child = root.with(&changes);            // parent + changes, structural sharing
let view = OverlayView::new(store.committed(), child.clone()); // layer first, then durable
view.sequence(BLOCKS).record(h); view.map(SPENT).range(&start, &end, limit); // same handles
view.durable();                              // the committed view alone (the seam)
let child = child.rebase(&store.committed());     // after a commit: what durable holds dropped
```

- `with` panics on a tip not above the layer's, another schema's tables, a map
  key the layer already holds, or a removal of one it already removed; the layer
  itself never changes.
- A layer's removal masks durable's value. Each map entry is owned by the
  newest block that wrote it, so `rebase` drops only entries that durable now
  holds and never brings back a value a later block removed.
- `rebase` drops every block through durable's tip; it panics when that tip is
  past the layer or not one of its blocks (another branch).
- `OverlayView::new` panics on a layer that is not above durable's tip (an
  un-rebased layer would read its blocks twice) or of another schema.
- `range` merges both runs and keeps the `None` = over `limit` rule. Durable is
  asked for `limit` plus the layer's removals in range, so removed rows never
  turn a fitting answer into `None`.

## Who drives a store

- Index writers fold each final block into `Store::changes(block)` and buffer
  it with `zaino_sync::apply` (`Store::apply`, counted); commits: the store's
  own at `write_buffer`, the writer's `zaino_sync::commit` after a finalized
  run and at `Shutdown` ([the writer loop](../zaino-sync/usage.md#writer-loop));
  a bulk fold reads its parent through `staged()`.
- Non-final blocks never reach a store: `zaino-nfs` holds one `Overlay` per index
  per block and serves `OverlayView::new(view(), layer)` from its snapshots
  ([`persistence-engine.md` §5](../../docs/design/persistence-engine.md#5-layers-and-writers)).

## Conformance suite (feature `testing`)

`conformance` tests any `PersistenceEngine` through the port alone: the store driven as a
writer drives it (`apply`, `commit`) under `Overlay`s kept as the NFS keeps them (`with`,
`rebase`). An engine implements `conformance::Subject`, which is `engine()` and `path()`
plus three optional hooks: `power_loss`, `settle` and `check`. It then runs
`conformance::history` under proptest and `conformance::contract` as a plain test (`contract`
includes `apply`'s own commit at `write_buffer`).
`PROPTEST_CASES=1000` is its heavy run. `conformance::Model` is the expected state for an
engine's own crash tests. Design:
[`persistence-engine.md` §4](../../docs/design/persistence-engine.md#4-tests).

## File layer (`fs`) and crash simulation

Everything the engine writes goes through `Arc<dyn Fs>` (`RealFs::shared()` in
production). `SimFs` (feature `testing`) is the in-memory implementation crash
tests run on:

- **`SimFs::recording()` + `crash_states()`:** every distinct state a power
  loss could leave at each persistence point, each tagged with what `set_tag`
  held then. A test reopens each state and asserts it recovered to an
  acknowledged or the attempted commit.
- **`fail_from(n)`:** fails the nth mutating call and every later one with
  `EIO`, applying nothing (Pebble `errorfs`). `mutations()` counts the calls,
  and `restarted()` is the same image after a process exit.
- **`fail_reads_from(n)`:** the same, for reads.
- **`power_loss()`:** the image a crash right now would leave.
- **`contents(path)` and `corrupt(path, edit)`:** inspect and damage files.
- **`DiskEngine::with_fanout(fs, n)` and `DiskStore::settle()`:** small fanouts
  and deterministic merge landing, for tests.
