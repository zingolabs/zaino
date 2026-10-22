# The persistence engine

How `zaino-persistence-lsm-tree` implements the `zaino-persistence` port: positional files for
sequence namespaces, a size-tiered LSM for map namespaces, and one manifest per index directory as
the atomic commit point. The port itself is described in its crate's usage guide; this document
covers what sits behind it.

Related: how the engine survives a crash ([durability.md](./durability.md)), removals in maps
([lsm-deletes.md](./lsm-deletes.md)), why every segment carries a filter
([index-data-structures.md](./index-data-structures.md)).

## 1. What the port asks for

Five properties hold for every namespace, and they are the contract the engine is built around:

1. **Final data only.** Non-final data stays in memory above the store, and reorgs never reach
   storage. Nothing on disk is ever rewound.
1. **Insert only, plus single deletes.** No update, no read-modify-write. A map declared with
   `deletes` may remove a key, at most once, and never re-insert it
   ([lsm-deletes.md](./lsm-deletes.md)). Every other namespace is insert only.
1. **One atomic commit per index, carrying the tip.** Every namespace of an index moves to the new
   tip together or not at all, however many commits were buffered. The tip only advances, and it
   is the resume point.
1. **Snapshot reads.** A reader handle holds one state for its whole life, every `scan_range`
   chunk included.
1. **Verifiable.** Every committed byte can be checked against integrity data, offline, and a
   mismatch is an error; nothing repairs.

And exactly two kinds of namespace:

- **Sequence**: records at dense positions `0, 1, 2, ...`, appended in order, keyed by
  `sequence_key(position)`. Here: a file and offset arithmetic.
- **Map**: fixed-width values under unique fixed-width keys, inserted in any order (removed once,
  under `deletes`), read by key or key range. Here: immutable sorted segments.

Because nothing is updated, the LSM needs no memtable, no write-ahead log and no versions: a
segment is one write's rows, sorted, and a merge never has to choose between two values of a key.

## 2. The port, mapped onto the engine

| Port                       | Here                                                                                                                         |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `LsmTreeBackend::open`     | lock the directory, read the manifest past the page cache, check name, format and namespaces, truncate to seals, map files    |
| `BackendWriter::commit`    | check the batch, add it to the write buffer, publish; write the buffer once its heap reaches `write_buffer`                   |
| `Backend::flush`           | write the buffer: every namespace written and sealed in parallel, then one manifest slot (the commit point)                   |
| `Backend::reader`          | the last published state: mapped files and segments at one manifest, plus the buffer as it was                               |
| `BackendReader::get`       | a sequence record by arithmetic; a map key from the buffer, then each segment whose filter admits it                          |
| `BackendReader::scan_range`| lazy chunks: positions for a sequence, a resumable k-way cursor over segments merged with the buffer for a map                |
| `MaintenanceReader::scan`  | `scan_range` drained into memory                                                                                              |
| `Backend::verify`          | every committed byte scrubbed against its page checksums                                                                      |
| `Backend::footprint`       | committed bytes and page-cache residency per namespace                                                                        |

For comparison, the same port over other engines:

| Port      | This crate                                                       | LMDB                                   | SQLite                                 |
| --------- | ---------------------------------------------------------------- | -------------------------------------- | -------------------------------------- |
| backend   | a directory + two-slot manifest                                  | an environment                         | a database file                        |
| sequence  | `<name>.dat` (+ `<name>.idx` end offsets if `Variable`)          | a database, integer keys, `MDB_APPEND` | a rowid table                          |
| map       | `<name>/` of sorted, filtered segments, merged in the background | a database                             | a `WITHOUT ROWID` table                |
| commit    | seal grown files and new segments, write a manifest slot         | one write transaction + sync           | one transaction (`synchronous = FULL`) |
| reader    | mapped files + segments at one manifest, plus the buffer         | a read transaction                     | a read transaction (WAL)               |
| range     | a seek + walk per segment the scope filter admits                | a cursor from `start`                  | `SELECT … WHERE key >= ? AND key < ?`  |
| verify    | page checksums                                                   | a full read                            | `PRAGMA integrity_check`               |

## 3. Writes

One writer at a time holds the writer state (a mutex): the engine, the write buffer and whether a
write failed. A commit:

1. **Checks its shape**, with no disk read: the tip above the last one, every namespace declared,
   widths, duplicates in the batch, deletes allowed, sequence positions (`check_shape`).
1. **Prefetches**: every map key the batch names, looked up in one batched pass over the segments
   (filters first, then the pages each hit needs).
1. **Checks key state** against the buffer, then that prefetch: present, absent or removed
   (`check_keys`). A refused batch applies nothing.
1. **Buffers** the batch: one `imbl` structure per namespace, so a clone is cheap and a published
   state shares every node a later commit does not touch. A key inserted and removed while both are
   buffered is a cancelled pair ([lsm-deletes.md](./lsm-deletes.md)).
1. **Publishes** the new state (an `ArcSwap` store); readers opened from then see it.
1. **Writes the buffer** once its estimated heap reaches `write_buffer`, then publishes the
   durable state over an empty buffer.

A write (`Engine::write`) first joins finished background merges and any the write would leave
too far behind, then writes every namespace in parallel: each sequence appends and seals its
files, each map sorts its rows into one new segment (and a tombstone-only segment for cancelled
pairs). Everything that can fail runs before the manifest commit
([durability.md](./durability.md) §4). A failed write refuses every later write until the backend
is reopened, except a background merge's failure that a retry can clear (§5 there).

## 4. Reads

A reader holds an `Arc` of one published state: the mapped sequence files and map segments at one
manifest, and the buffer as it was. It never touches the writer, so readers take no lock and scale
with cores; the state, and every file it maps, stays alive until the last reader drops it.

- **Sequences**: a fixed-width record is at `position × width`; a variable one is between two end
  offsets in `<name>.idx`. A record past the durable length is read from the buffer.
- **Map lookups**: the buffer first (a buffered removal masks the durable value), then every
  segment whose filter admits the key; a tombstone anywhere means absent.
- **Map ranges**: a `Cursor` walks each candidate segment's window of rows (a segment whose scope
  filter rejects the range is skipped) in a k-way merge, either direction, and merges in the
  buffer's rows. It keeps its position between chunks, so a `scan_range` never re-seeks.
- **Pages** are checked against their checksums the first time any read touches them; a segment's
  filter is checked whole when it is mapped.

## 5. Layout of the crate

```text
src/
  backend.rs    LsmTreeBackend, its reader and writer: the port
  buffer.rs     the write buffer (imbl), shared with published states
  engine.rs     Engine / DiskView: one manifest over both namespace kinds, verify, footprint
  sequence.rs   sequence namespaces as positional files
  lsm/          map namespaces as size-tiered sorted segments
  pages.rs  manifest.rs  dir.rs    checksummed files, the commit point, the directory and its lock
  fs.rs  fs/    the Fs seam: RealFs, SimFs (feature `testing`), and fs/sys.rs, the crate's only unsafe
  error.rs      internal errors and their mapping onto the port's
  emit.rs       metrics
```

## 6. Tests

The port's behaviour is tested through the port: `zaino_persistence::conformance` (feature
`testing`) holds the suite every backend runs, and this crate runs it on `SimFs` (power loss =
`SimFs::power_loss`, every reopen verified clean) and on the real filesystem.

- `history`: random commits over every namespace shape (a variable sequence, fixed ones, a scoped
  map with deletes, a point-lookup map), flushes, reopens and power loss, with reads, chunked
  scans in both directions and pinned readers checked against a `BTreeMap` / `Vec` model after
  every step.
- `contract`: every commit rule refused with its own error and nothing applied, undeclared
  namespaces, width checks, tips, footprint and verify.

Then what only this engine can show: every crash state of a commit history, every failed I/O call,
every failed read at open, its invariant checks firing (a key held twice, a corrupt merge input),
failed and corrupt merges, leftover cleanup and its alerts, open's trimming and refusals, a changed
scope, the manifest body's golden bytes, the 1 MiB slot outgrown, and verify (bad page, lost file,
a file a merge retired mid-scrub). The LSM's own tests cover its layout arithmetic, prefetch plans,
filters, merges, bandwidth and the cursor.
