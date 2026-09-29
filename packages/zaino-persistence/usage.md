# zaino-persistence

The storage core the index crates share: the file layer, the manifest commit
point, page checksums (the one disk-integrity check every index file carries),
and immutable sorted segments. The protocol these implement is
[`docs/design/durability.md`](../../docs/design/durability.md).

Each index owns its own record layouts: fixed-width `encode`/`decode` functions
beside a golden-bytes test (e.g. `zaino-index-tree-state/src/heights.rs`).

## File layer (`fs`)

Every index reads and writes through `Arc<dyn Fs>`:

```rust
use zaino_persistence::fs::{Fs, RealFs};

let fs = RealFs::shared();               // std::fs + memmap2 (Linux)
let file = fs.open(&path)?;              // read + write, created if absent, never truncated
file.write_all_at(&bytes, offset)?;      // positional only: no implicit cursor
file.sync_data()?;                       // content + length durable
fs.sync_dir(&dir)?;                      // new names / renames / removals durable
let mapping = file.map()?;               // read-only Bytes; None when empty
```

- `lock(path)` takes an exclusive `File::try_lock`; a held lock is
  `ErrorKind::WouldBlock`.

`SimFs` (feature `testing`) is the in-memory implementation crash tests run on.
`SimFs::recording()` records the image around every persistence point
(`sync_data`, `sync_dir`, `rename`, `remove`); `crash_states()` then returns
every distinct state a power loss there could leave, each tagged with the value
`set_tag` held at the time (a test's acknowledged commits):

```rust
let fs = SimFs::recording();
// ... run a workload, fs.set_tag(n) after each acknowledged commit ...
for state in fs.crash_states() {
    let store = Store::open(state.fs, path, network)?;   // must reopen
    // recovered = commit `state.tag` or `state.tag + 1`
}
```

Crash states cover unsynced writes dropped, kept as a prefix, reordered (one lost,
later ones kept), torn, zero- or garbage-filled, and unsynced directory entries
lost or kept.

`fail_from(n)` fails the nth mutating call (create, write, truncate, sync,
rename, remove; 0-based) and every later one with `EIO`, applying nothing
(Pebble `errorfs`). Loop `n` upward until the workload succeeds; `mutations()`
counts the calls so far, and `restarted()` is the same image after a process
exit, healthy again. `power_loss()` is the image a crash right now would leave
(only synced entries and bytes), even while background merges are mid-write:

```rust
for n in 0.. {
    let fs = SimFs::new();
    fs.fail_from(n);
    match workload(&fs) {
        Ok(()) if fs.mutations() <= n => break,  // op n never reached
        outcome => /* the Err names "injected EIO at op n" */,
    }
    let store = Store::open(fs.restarted(), path, network)?; // recovers a committed state
}
```

`contents(path)` and `corrupt(path, edit)` inspect and damage files durably.

## Page checksums (`pages`)

Every index file is a `PagedFile`: append-only data, a `<file>.crc` sidecar
holding a CRC-32 per complete 4 KiB page, and a `Sealed { len, tail }` (length +
CRC of the partial last page) that the owner's manifest carries.

```rust
use zaino_persistence::pages::{PagedFile, Sealed};

let mut file = PagedFile::open(fs.as_ref(), &path, body.sealed)?; // fresh = Sealed::EMPTY
file.append(&bytes)?;                    // at the end, never positional
let sealed = file.seal()?;               // fsync data, write + fsync new page CRCs
// commit a manifest carrying `sealed`, then:
let pages = file.pages(sealed, Some(&old_pages))?; // read view; checked pages stay checked
let bytes = pages.bytes(range);          // zero-copy, every page checked on first touch
```

- `open` = the file at least as long as its seal (then truncated to it) and the
  tail page's CRC: `PageError::{Lost, Tail}`. Nothing else is read.
- A read that first touches a page whose CRC disagrees **panics** (corruption:
  zainod aborts; never serves bytes it cannot vouch for).
- `Pages::open(fs, path, sealed, access)` maps an immutable sealed file (a segment)
  without truncating. `fs::Access` is the kernel readahead advice for that mapping
  and its checksums, per mapping and never per file: `Random` (point lookups: one
  4 KiB fault instead of a 128 KiB readahead window), `Sequential` (one pass;
  larger readahead, pages reclaimed sooner), `Normal` (no advice). `PagedFile`
  views (appendable files) are `Normal`.
- `scrub(dir, relative, sealed)` is the offline check: plain sequential reads,
  every page against its CRC → `Scrub { committed_bytes, orphaned_bytes, lost,
  bad_pages }`. Each index exposes `committed_files(dir, network)` →
  `CommittedFiles { heights, files }`, the list `zainod verify` scrubs.

## Manifest and index directory (`manifest`, `dir`)

`IndexDir::open(fs, path, identity)` creates and durably links the directory,
takes its `LOCK`, drops a staged `MANIFEST.next`, and returns the committed
manifest body (`None` = never committed):

```rust
let Opened { dir, body } = IndexDir::open(fs, path, Identity { kind, format, network })?;
match body {
    Some(body) => /* decode, open every file at its seal */,
    None => { dir.ensure_empty("data.bin")?; dir.commit(&empty_body)?; }
}
dir.commit(&new_body)?;   // MANIFEST.next → fsync → rename → fsync dir
```

- The header checks magic, CRC, index kind, format version and network; any
  mismatch is a `ManifestError`.
- Every body starts with `Committed { extent, tip }`; `BodyReader` reads the
  rest with bounds checks, and `finish()` refuses trailing bytes.
- `ensure_empty` / `ensure_empty_dir` refuse data in a directory with no
  manifest (`Unmanifested`): a crash never produces it, because the first
  commit precedes any data write.
- `manifest::read(dir, identity)` = the committed body read offline (no lock).

## LSM segments

`lsm` is a size-tiered LSM for associative data that only grows. Every row
derives from one block and is never updated or deleted, so there is no memtable,
WAL, tombstone or version. Each batch is sorted and written once as
a segment (`<dir>/<id:010>.seg` + `.crc`): packed records, one fence key per ~4 KiB
block, and for a probed key a sharded BinaryFuse8 filter. The owner's manifest
lists committed segments as `SegmentMeta { id, records, sealed }`; a segment it does not
list is uncommitted and removed at open.

An index stored this way implements `LsmIndex` on a marker type and lets
`LsmStore` own its directory: the manifest (committed extent, tip hash, one
segment list per set), the fresh-directory sequence, and the commit.

```rust
use zaino_persistence::lsm::{LsmIndex, LsmStore, SegmentLog};

struct MyIndex;
impl LsmIndex for MyIndex {
    const KIND: IndexKind = IndexKind::ValueBalance;
    const FORMAT: u16 = 1;
    const SETS: &'static [&'static str] = &["outputs"];  // one sub-directory per set
    type Logs = SegmentLog<MyRow>;                        // or (SegmentLog<A>, SegmentLog<B>)
    // optional: const FANOUT (default 8), fn check(committed, lists) (manifest invariant)
}

let mut store = LsmStore::<MyIndex>::open(fs, path, network)?; // unlisted segments removed
let set = store.sets();                                        // read handles (shared)
store.commit(rows, extent, tip)?;  // segment per set → MANIFEST (commit point) → published
let committed = store.committed(); // Committed { extent, tip }
lsm::committed_files::<MyIndex>(path, network)?;               // for `zainod verify`
```

Failures are `zaino_persistence::StoreError` (`Io`, `Manifest`, `Page`,
`Segment`), the one error every index directory reports.

- A `commit` that returns `Err` ends the store: any later `commit` panics (an
  `fsync` error is never retried; `docs/design/durability.md` §6). Drop it and
  reopen; recovery lands on the last durable manifest.
- `commit` asserts `extent` past the committed one, and runs `LsmIndex::check`
  on the lists it is about to write as well as on the lists it reads at open.

The pieces underneath, for a store with a different layout:

```rust
use zaino_persistence::lsm::{SegmentLog, SegmentSet};

let set = SegmentSet::<MyKey>::open::<MyRow>(fs, &dir, &body.segments)?; // unlisted removed
let mut log = SegmentLog::<MyRow>::open(set.clone(), 8);                 // fanout 8

let listed = log.batch(rows)?; // sorted, written, sealed, linked + finished merges: the next list
// commit the manifest carrying `listed`, then:
log.committed()?;              // publish, unlink merged-away inputs, launch merges

let rows: Vec<MyRow> = set.pin().range(&from, &to); // [from, to), ascending
let row: Option<MyRow> = set.pin().get(&key);       // exact key: filter first
let rows: Vec<Option<MyRow>> = set.pin().get_many(&keys); // a batch, in `keys`' order
let rows: Option<Vec<MyRow>> = set.pin().range_at_most(&from, &to, limit); // None = > limit rows
```

- `range_at_most` stops scanning at row `limit + 1` and answers `None` (never a
  truncated list): a serve-path budget bounds the cost, not the range's size.
  `range` = `range_at_most(.., usize::MAX)`.

- `get` probes the newest segment first. The list is roughly data age (batches
  append, a merge takes its oldest input's slot) and lookups skew recent. Keys
  are unique across segments, so order never changes an answer.
- `get_many` is the batched lookup (RocksDB `MultiGet`): it sorts the keys and
  resolves them in key order. Neighbouring keys share fence, filter and record
  pages, so one fault serves several. From 64 keys it resolves contiguous sorted
  runs in parallel on the rayon pool (a cold batch keeps many reads in flight);
  below that it stays on the calling thread, where rayon's wake-up cost 5–10×
  the lookups themselves (measured). Use it whenever one request needs many keys.
- Readers map a probed set's segments `Access::Random` (point lookups) and an
  unprobed set's `Access::Normal` (range scans want readahead). A merge maps its
  inputs separately with `Access::Sequential`, so it neither slows lookups nor is
  slowed by them.

- `Key` (`const LEN`, `encode`, `decode`) must be **big-endian**: reads compare
  encoded prefixes, so byte order must be key order. `const PROBED = true` gives
  each segment a filter and requires the encoded key's first 8 bytes uniform (a
  hash, a txid): the filter shards on them.
- `Record` (`type Key`, `const STRIDE`, `key`, `encode`, `decode`) is
  fixed-width, key first. A batch with a duplicate key panics (asserted while
  writing, before anything is sealed). Key length > 0, ≤ `STRIDE`, and ≥ 8 when
  probed are checked at compile time.
- Keys are unique across a set's committed segments (the owner's invariant):
  `range` panics on a key it finds in two segments, and a merge of two such
  segments panics on its thread, resumed at the next `batch()`.
- A merge's output must hold exactly its inputs' row count (asserted when it
  lands). Under `cfg(test)` or feature `testing`, every sealed segment (batch or
  merge) is read back through its page checksums: row count, strictly ascending
  keys, no filter false negative, and a CRC of the rows equal to the one taken
  while writing (RocksDB `paranoid_file_checks`).
- Merges run in the background, off the commit path (LevelDB/RocksDB
  background compaction). `committed()` starts one per size tier
  (`⌊log_fanout(records)⌋`) that lists `fanout` idle segments, taking the oldest of
  the lowest such tier first, on its own `merge <set> t<tier>` thread. A merge
  streams its inputs through their page checksums and seals and links its
  output, but never commits it: the next `batch()` swaps each finished merge in
  for its inputs, and that batch's manifest commits both at once. Inputs are
  unlinked only after that manifest is durable. Peers need not be adjacent,
  since keys are unique across segments and list order means nothing to readers.
- One merge per tier at a time, so a small merge never waits behind a large
  one. Write stall: if a merging tier falls two idle windows behind
  (`STALL_WINDOWS`), `batch()` waits for that merge (RocksDB
  `level0_stop_writes_trigger`), which bounds how many segments a read fans out
  over.
- Errors surface at the next `batch()`, and a merge panic (a corrupt input
  page) resumes there. Dropping the log cancels and joins every merge; a
  cancelled output is unlisted, so the next open removes it.
- Feature `prometheus` publishes `zaino_lsm_*` metrics labelled by `set` (the
  segment directory's name). They cover segments and running merges per size
  tier, the stall count, rows batched and merged (their ratio is write
  amplification), merge bytes, and merge and stall durations. Register them with
  `lsm::describe_metrics` and `lsm::METRIC_BUCKETS`.
- Logs (`tracing`, fields `set`, `tier`, `rows`, `size`):
  - `Compacting segments` (debug, adds `segments`) when a merge starts.
  - `Compacted segments` (debug, adds `took`) once the manifest listing its
    output is durable and its inputs are unlinked.
  - `Commit waited on compaction` (warn) when a stall joins a merge.
  - A merge thread runs inside the span that opened or batched the log, so its
    lines carry the owner's context.
- Every `PagedFile` starts writeback per 1 MiB appended
  (`FileHandle::write_behind` = `sync_file_range(SYNC_FILE_RANGE_WRITE)`,
  RocksDB `bytes_per_sync`). A merge's seal-time `fsync` then finds little
  dirty data, so it never stalls the commit path's own `fsync`s.
- A seek = binary search over the fences, then within one block (≈ one page).
- A probe reads its filter shard whole (up to ~290 pages). The first probe of a
  shard CRC-checks all of it; later probes read it through a per-shard flag, not
  a per-page walk. A corrupt shard still dies on its first probe.
- Filter sizing (BinaryFuse8, ≤ 2²⁰ keys per shard):
  [`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §7.
