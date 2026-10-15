# zaino-persistence

Storage for every Zaino index. It has two halves: the **port** (`port.rs`), the small set of
traits an index is allowed to use, and **`DiskEngine`**, the one engine behind it. Read this if you
are writing or reviewing an index, or debugging what is on disk.

## What an index may ask

Every index is a fold over final blocks, so the port promises only five things:

1. **Final data only.** Non-final blocks live in memory (`Overlay`, below). Reorgs never reach a
   store, so nothing on disk is ever rewound.
2. **Insert only.** Sequence records are appended; map keys are inserted once. The single
   exception is a map that declares `deletes()` (see [Deletes](#deletes)).
3. **One atomic commit per index, carrying the tip.** Every table moves to the new tip together or
   not at all. The committed tip is where the index resumes.
4. **Snapshot reads.** A `View` never changes while held, so a request or stream sees one state.
5. **Verifiable.** Every committed byte can be checked offline, and corruption stops the process.

There are two kinds of table:

- **Sequence**: records at dense positions `0, 1, 2, …` (a height, a tree slot). Found by
  arithmetic, never searched.
- **Map**: values under unique byte keys, inserted in any order, read by key or key range.

```rust
const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);
const SPENT: MapTable = MapTable::point(0, "spent", Width::fixed(36), Width::fixed(36));
pub const FORMAT: u16 = 1;
pub const TABLES: Tables = Tables::new(&[BLOCKS], &[SPENT]); // ids = positions, checked at compile time

let schema = Schema::new(IndexKind::CompactBlock, FORMAT, network, TABLES);
let mut store = DiskEngine::new(fs, LsmConfig::default()).open(path, &schema, WRITE_BUFFER)?;

let mut changes = store.changes(block.at());           // one block's empty delta
changes.sequence(BLOCKS).append(&record);
changes.map(SPENT).insert(&outpoint.encode(), &spend);
store.apply(changes);                                  // buffered: visible in applied() only
store.commit()?;                                       // durable, then visible in durable()

let view = store.durable();                            // what serving pins
view.sequence(BLOCKS).records(a..b);                   // zero-copy slices of the mapping
view.map(SPENT).range(&start, &end, limit);            // [start, end); None = more than `limit`
```

An index owns only its `FORMAT`, its `TABLES` and its record layouts (named `encode` / `decode`
functions beside a golden-bytes test). The engine sees bytes. The details that bite:

- **Map keys compare as bytes**, so a number in a key is big-endian (the root `CLAUDE.md` byte
  order rule: locators big-endian, quantities little-endian). Keys must lead with at least 8
  uniformly distributed bytes (a hash, a txid): the segment filter shards on them.
- **`MapTable::point` vs `MapTable::scoped`.** A point map is read by whole key. A scoped map
  declares its `scope`: the number of leading key bytes every range read shares (an address, say).
  The filter covers that prefix, so a range scan only visits segments that may hold it.
- **`cache_writes()`** marks a table a fold reads back (tree-state, utxo-set's `outputs`).
  Everything else is dropped from page cache once synced, so bulk writes never evict hot tables.
- **`apply` vs `commit`.** `apply` buffers in RAM; a writer folding the next block reads its parent
  through `applied()` (committed + buffered). Once the buffer's heap (`DiskStore::buffered_bytes`)
  reaches the `write_buffer` given to `open`, `apply` commits by itself. `commit` writes everything
  buffered at the last applied tip. `Schema::changes(at)` (or `store.changes(at)`) opens a block's
  delta. Point reads through `applied()` hit the buffer's hash index; a map `range` through it
  scans and sorts every row the buffer holds for that map, so a fold should not range-read its own
  buffered map.
- **A failed commit poisons the store.** Any later `apply` or `commit` panics. Drop the store and
  reopen it: recovery is always a fresh `open` from the last durable manifest, never a retried
  `fsync`. `commit`'s `CommitFailed` names the index and its directory (a full disk anywhere in the error
  chain reads as "disk … full"); `CommitFailed::crash` turns it into that panic.
- **Reads never return errors.** Reading past what was committed is a bug, and a page that fails its
  checksum panics on first touch with the remedy (`zainod verify`, resync).
- **Misuse panics where it happens**, naming the table: a handle from another schema, a fixed-width
  item of the wrong size, a tip that does not advance, a key inserted twice, a table the engine
  cannot hold (for `DiskEngine`, see [its LSM's limits](#on-disk)).
- **`IndexKind::name()`** (`compact_block`, …) is the one spelling of an index: metric label,
  `/status` index name, default directory, task name.

## Deletes

A map declaring `MapTable::deletes()` accepts `changes.map(T).remove(&key)`. The contract, per
key: inserted at most once, removed at most once, never re-inserted, and never both inserted and
removed by one block. This is RocksDB's `SingleDelete`, and it keeps everything simple: a key has
at most one value row and one tombstone row anywhere, so no answer depends on segment order.

- A removed key reads absent from `value`, `values` and `range` (whose `limit` counts live rows).
- A removal of a key still in the write buffer cancels there; neither row reaches disk.
- Otherwise it is written as a tombstone row. A point read checks every segment whose filter admits
  the key, and a tombstone in any of them means absent.
- A merge holding a key's value and its tombstone drops both; a lone tombstone is kept. Any other
  duplicate is `SegmentError::Contract`, surfaced at the next commit.
- In an `Overlay` a removal masks the durable value, and `rebase` never brings it back.
- Rows of a `deletes()` map carry one flag byte; every other map's layout is unchanged.

utxo-set's `outputs` is the one user: it keeps only unspent outputs, so its fold's prevout
lookups fit in page cache.

## Non-final data: `Overlay`

Data above a durable tip has the same shape as a store's buffer. `zaino-nfs` keeps one `Overlay`
per index per non-final block; a clone is a few pointer copies (`imbl`), so a child block shares
its parent's layer.

```rust
let child = parent_layer.with(&changes);                       // parent + one block
let view = OverlayView::new(store.durable(), child.clone()); // layer first, then durable
let child = child.rebase(&store.durable());                   // after a commit: drop what is durable
```

`with`, `rebase` and `OverlayView::new` panic on anything that would read a block twice or on the
wrong branch (a tip not above the layer's, an un-rebased layer, a durable tip off the layer's
blocks).

## On disk

```text
<dir>/LOCK              exclusive lock for the life of the store
<dir>/MANIFEST          two 64 KiB slots: the commit point
<dir>/<seq>.dat         sequence records back to back       (+ .crc: CRC-32 per 4 KiB page)
<dir>/<seq>.idx         Variable sequences: u64 end offset per record
<dir>/<map>/<id>.seg    one immutable sorted segment per commit or merge (+ .crc)
```

**Sequences** are append-only positional files: record `i` of a `Fixed(n)` table is at `i × n`, a
read is one slice of the mapping, and a height range is one contiguous span.

**Maps** are a size-tiered LSM. Each commit sorts each map's buffered rows into one new segment:
fixed-width rows packed back to back, a fence key per 4 KiB block, and a binary fuse filter (false
positive rate 2⁻⁸). A lookup checks each segment's filter, then reads one page of fences and one
block of rows. Once a size tier holds `fanout` segments (`LsmConfig`, default 16), a background
thread merges them at low CPU and I/O priority, within an engine-wide cap (`merge_slots`,
`merge_mib_per_sec`). A merge commits nothing itself: the next commit's manifest lists its output,
and the inputs are deleted only after that manifest is durable. A commit waits for a merge only
when its tier has fallen two windows behind, which bounds how many segments a read searches.

Rows are fixed-stride, so the LSM holds only `Width::Fixed` keys and values, and its filter shards
on 8 key bytes, so a map filters at least 8 (its whole key when `point`, else its `scope`). These
are the LSM's limits, not the port's (`Width::Variable` maps are expressible for another engine):
`DiskEngine::open` panics on a map outside them, naming it.

## The commit and crash contract

Everything Zaino stores is derived from the chain, so the worst a failure can cost is a resync. The
engine therefore never repairs: it either opens a state it can prove, or refuses.

A commit:

1. appends each sequence's records, writes each map's new segment, every table in parallel;
2. **seals** each file it grew: data synced, then the checksums of newly completed pages (on ext4
   or XFS, sequence files are written back and covered by one device flush instead of an fsync
   each; `fs/probe.rs` decides at open);
3. writes the new `MANIFEST` slot and syncs it. **This is the commit point.**

The manifest holds the committed tip (height and hash), each sequence file's seal (length, tail
page checksum, digest of its `.crc`) and each map's segment list. Bytes the manifest does not claim
are uncommitted, whatever they contain. Commit `n` overwrites slot `n % 2`, so the previous commit
always survives a torn write, and a slot that fails its CRC is read as a crash mid-commit.

Opening a store:

- refuses a manifest written for another index kind, `FORMAT` or network
  (`ManifestError::{Kind, Format, Network}`), and data in a directory with no manifest
  (`Unmanifested`);
- truncates each sequence file to its seal and deletes segments the manifest does not list;
- refuses a file shorter than its seal, a tail page that fails its checksum, or a `.crc` file whose
  digest differs from the manifest's (`PageError::{Lost, Tail, Sums}`).

Every full 4 KiB page has a CRC in `<file>.crc`, seeded with the page's index; the manifest pins the
`.crc` file through its digest. A stale page with its stale checksum (a lost write, a file copied in
from elsewhere) therefore still fails. Pages are checked on first read, not at open, so open costs
reading the `.crc` files (1/1024 of the data).

zainod pairs this with a fail-stop policy: any panic aborts the process, nothing restarts in
process, and the service manager's restart is the recovery path.

## Why append-only files and mmap

The data is dense, positional and written once, so a key-value store only adds overhead, and an
append-only file stays unfragmented where a copy-on-write B-tree under random inserts does not.

## mmap hazards, and how the code avoids them

- **SIGBUS on truncation.** Touching a mapped page past a shrunken file kills the process with no
  unwind. Files are truncated only in `open`, before anything maps them, and `LOCK` keeps a second
  process from opening (and truncating) the same directory.
- **SIGBUS on a full disk.** Nothing is ever mapped writable; all writes go through the file
  descriptor, so `ENOSPC` is an ordinary `io::Error` and `msync` never enters the picture.
- **Mappings longer than the commit.** A mapping covers the whole file, which may include an
  interrupted commit or the zeroed reserve. Every read goes through `Pages`, sliced to the sealed
  length.
- **Page faults on async threads.** A cold read can block for milliseconds. Callers run reads on the
  blocking pool (`zaino-grpc`'s read lanes), and multi-record reads issue `MADV_WILLNEED` first. Point-lookup segments are mapped `MADV_RANDOM`,
  merge inputs `MADV_SEQUENTIAL`.
- **Zero-copy slices outlive the view.** `Bytes` returned by a read hold the mapping alive, which is
  sound only because committed bytes are never rewritten or truncated after publication.

## Verifying offline

```rust
let report = DiskEngine::new(fs, LsmConfig::default()).verify(path, &schema)?;
report.is_clean();   // report.units: per file, committed / orphaned bytes, lost, bad pages
```

Reads every file the manifest seals, page by page, with plain reads: no mapping, no lock, safe
beside a running daemon. If a merge retires a segment mid-scrub, the scrub restarts against the
newer manifest. `zainod verify` wraps this.

## Observability

- `zaino.store.commit_seconds` / `commit_bytes_total` per index (label `index`); `zaino.lsm.*` per
  map (labels `index` + `table`: segments per tier, merges, stalls, cancelled tombstone pairs).
  Register with `describe_metrics` and the `METRIC_BUCKETS` constants.
- `DiskView::footprint()`: per table, committed bytes and how many are in page cache (`mincore`).
- Logs: `Compacting segments` / `Compacted segments` (debug), `Commit waited on compaction` (warn).

## Testing

Everything goes through the `Fs` trait: `RealFs` in production, `SimFs` (feature `testing`) in
tests. `SimFs` tracks what is durable versus merely written, and enumerates every state a power loss
could leave: unsynced writes dropped, reordered, torn on 512-byte sector boundaries, or lost in one
file while kept in others. `fail_from(n)` returns `EIO` from the nth mutating call on.

`DiskEngine`'s tests live in `src/disk/tests.rs` and are its own. Each `PersistenceEngine`
implementation writes and owns its tests; there is no shared harness.

- **Model test** (`random_histories_answer_like_a_btreemap_through_merges_reopens_and_power_loss`):
  random blocks, reorgs, commits, merges, reopens and power loss on `SimFs`, checked against
  `Vec` / `BTreeMap` models after every step.
- **Guards** (`a_key_inserted_twice_by_one_block_panics`, …): one `#[should_panic]` test per
  precondition panic.
- **Files-only tests**: every crash state of a commit history, every failed I/O call, open's
  trimming and refusals, the manifest body's golden bytes, and `verify`.

After any change to this crate: the [heavy run](../../docs/testing.md#heavy-run) (required
before the change is done; `CLAUDE.md`).
