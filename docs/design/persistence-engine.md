# The persistence engine

One port between every index and its storage (`zaino-persistence/src/port.rs`), and the engine
Zaino ships behind it, `DiskEngine`: positional files for sequences, an LSM for maps. The port
names only what any database can provide: tables, atomic commits, snapshots, verification. A
B-tree (LMDB) or SQL (SQLite) engine would implement the same traits; nothing in the port belongs
to one engine.

Related: what each index keeps ([index-data-structures.md](./index-data-structures.md)), how the
engine survives a crash ([durability.md](./durability.md)), why it is files and segments rather
than a B-tree ([persistence-architecture.md](./persistence-architecture.md)).

## 1. What every index needs

| Index               | Tables                                                                       | Writes                 | Reads                                               |
| ------------------- | ---------------------------------------------------------------------------- | ---------------------- | --------------------------------------------------- |
| compact-block       | one record per height, variable size                                         | appends                | by height; a run of heights, streamed               |
| tree-state          | per-height records 48 B; per pool 32 node arrays 32 B + subtree roots 36 B   | appends                | by position, scattered; a run of subtree roots      |
| header-chain        | per-height records 56 B                                                      | appends                | by height; the last few                             |
| transparent-address | `receives` (address ‖ height ‖ outpoint → value), `spent` (outpoint → spend) | inserts, any key order | key range within one address; batched point lookups |
| block-hash          | hash → height                                                                | inserts                | point lookup                                        |
| value-balance       | outpoint → value                                                             | inserts                | batched point lookups (by its own writer)           |

Five properties hold for all of them, and they are the contract:

1. **Final data only.** Non-final data stays in memory above the store (`Tiered`, §5), and reorgs
   never reach storage. The LMDB store this replaced deleted and rewound on disk, which needed the
   whole block back to reverse every secondary index.
1. **Insert only.** No update, no delete, no read-modify-write.
1. **One atomic commit per index, carrying the tip.** Every table of an index moves to the new tip
   together or not at all. The tip only advances, and it is the resume point.
1. **Snapshot reads.** A reader holds one committed state for a whole request or stream.
1. **Verifiable.** Every committed byte can be checked against integrity data, offline, and a
   mismatch stops the process; nothing repairs.

And exactly two kinds of table:

- **Sequence**: records at dense positions `0, 1, 2, ...`, appended in order (a height, a tree
  slot, a subtree index). SQLite: a rowid table. LMDB: integer keys written with `MDB_APPEND`. A
  file: offset arithmetic.
- **Map**: values under unique keys, inserted in any order, read by key or key range. SQLite: a
  `WITHOUT ROWID` table. LMDB: a database. An LSM: sorted segments.

## 2. The port

```rust
pub trait PersistenceEngine: Send + Sync + 'static {
    type Store: Store;
    fn open(&self, path: &Path, schema: &Schema) -> Result<Self::Store, StoreError>;
    fn verify(&self, path: &Path, schema: &Schema) -> Result<Verification, StoreError>;
}

pub trait Store: Send + 'static {
    type View: View;
    fn schema(&self) -> &Schema;
    fn path(&self) -> &Path;
    fn view(&self) -> Self::View;
    fn commit(&mut self, changes: Changes) -> Result<Self::View, StoreError>;
}

pub trait View: Clone + Send + Sync + 'static {
    fn tip(&self) -> Option<BlockRef>;
}

pub trait SequenceRead: View {
    fn len(&self, table: SequenceId) -> u64;
    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes>;
    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes>;
}

pub trait MapRead: View {
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes>;
    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>>;  // in `keys` order
    fn range(&self, table: MapId, start: &[u8], end: &[u8], limit: usize)
        -> Option<Vec<(Bytes, Bytes)>>;                                     // None = over `limit`
}
```

An index declares its tables once, and builds each commit against them:

```rust
let schema = Schema::new(IndexKind::TransparentAddress, FORMAT, network)
    .with_map(RECEIVES, "receives", Width::fixed(61), Width::fixed(8), 21)
    .with_map(SPENT, "spent", Width::fixed(36), Width::fixed(36), 0);
let mut store = DiskEngine::new(fs).open(path, &schema)?;

let mut changes = Changes::new(tip, store.schema());
changes.insert(SPENT, &outpoint.encode(), &encode_spend(&spend));
let view = store.commit(changes)?;
```

- **`Width`** is `Fixed(NonZeroU32)` or `Variable`; ids (`SequenceId`, `MapId`) are declared in
  order and index the schema.
- **`scope`** is the one hint: the leading key bytes every range read shares (0 = point lookups).
  It is a partition key, a general database idea; an engine may ignore it. Map keys compare as
  bytes and lead with at least 8 uniform bytes (a hash, a txid).
- **`Changes`** owns one buffer per table, shaped by the schema: a fixed-width table holds its
  bytes back to back with no per-item overhead, a variable one adds an end offset per item. Widths
  are checked as each item arrives, so a wrong one panics at the call that made it, naming the
  table. Callers encode into temporaries and never manage a lifetime.
- **Commit** makes every change and the tip durable together, then returns the new view. The tip
  must advance. An `Err` poisons the store: every later commit panics, and recovery is a reopen
  (a failed sync is never retried).
- **Reads** never return errors: a read past what was committed is a bug, and corruption panics on
  the first touch of a page whose checksum fails.

Invariants are enforced by whoever owns them, at the earliest point, as panics naming the table (a
schema is a constant in the index's code, so a mismatch is a bug, not a runtime condition):

| Where                          | Panics on                                                                                  |
| ------------------------------ | ------------------------------------------------------------------------------------------ |
| `Schema::with_*`               | ids out of declaration order                                                               |
| `Changes::append` / `insert`   | an undeclared id; a fixed-width item of the wrong size                                     |
| the LSM (`Shape::of`, at open) | a `Variable` key or value; a scope longer than the key; under 8 filtered key bytes         |
| the LSM (each batch)           | a row of the wrong widths; a duplicate key                                                 |
| sequence files (each append)   | a fixed-width record of the wrong size                                                     |
| `Store::commit`                | changes built for another schema; a tip that does not advance; a commit after a failed one |

| Port      | `DiskEngine`                                                     | LMDB                                   | SQLite                                 |
| --------- | ---------------------------------------------------------------- | -------------------------------------- | -------------------------------------- |
| store     | a directory + two-slot manifest                                  | an environment                         | a database file                        |
| sequence  | `<name>.dat` (+ `<name>.idx` end offsets if `Variable`)          | a database, integer keys, `MDB_APPEND` | a rowid table                          |
| map       | `<name>/` of sorted, filtered segments, merged in the background | a database                             | a `WITHOUT ROWID` table                |
| commit    | seal grown files and new segments, write a manifest slot         | one write transaction + sync           | one transaction (`synchronous = FULL`) |
| view      | mapped files + segments at one manifest                          | a read transaction                     | a read transaction (WAL)               |
| `records` | one mmap span, sliced per record                                 | a cursor walk                          | `SELECT … WHERE rowid BETWEEN`         |
| `range`   | a seek + walk per segment the scope filter admits                | a cursor from `start`                  | `SELECT … WHERE key >= ? AND key < ?`  |
| verify    | page checksums (`std::fs`, read-only)                            | a full read                            | `PRAGMA integrity_check`               |

What the port deliberately does not have:

| Not in the port                  | Because                                                                                                                |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| delete, update, truncate, rewind | final data is never wrong, so never changed                                                                            |
| transactions, cursors, iterators | a commit is the only write and a `View` the only read                                                                  |
| a metadata blob per commit       | anything an index resumes from is a record at its tip (compact-block's tree sizes = its last record's `ChainMetadata`) |
| async                            | reads are bounded; callers run them on read lanes or the blocking pool                                                 |
| engine knobs (fanout, page size) | the engine's own configuration, never an index's                                                                       |
| `dyn`                            | `View: Clone` and indexes name the one engine; switching engines is a later, separate change                           |

## 3. Layout of the crate

```text
zaino-persistence/src/
  port.rs       the traits, Schema, Changes, Verification
  tiered.rs     Tiered / TieredView: blocks above the committed tip, over any Store (§5)
  disk.rs       DiskEngine / DiskStore / DiskView: one manifest over both table kinds
  sequence.rs   sequence tables as positional files
  lsm/          map tables as size-tiered sorted segments
  pages.rs  manifest.rs  dir.rs  fs.rs    checksummed files, the commit point, the Fs seam
```

Sequences default to files and maps to the LSM; choosing an engine per index is future work and
would add engines beside `DiskEngine`, not change the port.

| Index               | Schema                                                                                      |
| ------------------- | ------------------------------------------------------------------------------------------- |
| compact-block       | `blocks`: Variable                                                                          |
| tree-state          | `heights`: Fixed(48); per pool `<pool>/l00`..`l31`: Fixed(32), `<pool>/subtrees`: Fixed(36) |
| header-chain        | `headers`: Fixed(56)                                                                        |
| transparent-address | `receives`: Fixed(61) → Fixed(8), scope 21; `spent`: Fixed(36) → Fixed(36)                  |
| block-hash          | `by_hash`: Fixed(32) → Fixed(4)                                                             |
| value-balance       | `outputs`: Fixed(36) → Fixed(8)                                                             |

Each index keeps only its record layouts (named encode/decode functions beside golden tests) and
its schema. Manifest bodies, seals, empty-directory checks, snapshots and committed-file lists are
the engine's. `zainod verify` runs `DiskEngine::verify` with each enabled index's schema.

## 4. Tests

The port's behaviour is tested through the port: `zaino_persistence::conformance` (feature
`testing`) holds a suite every engine runs against itself, so a heavy run is the same command for
any adapter.

```rust
impl conformance::Subject for MyEngineUnderTest {
    type Engine = MyEngine;
    fn engine(&self) -> MyEngine { … }             // over the storage as it stands
    fn path(&self) -> &Path { … }
    // optional hooks, no-op by default:
    fn power_loss(&mut self) -> bool { … }          // swap in what a crash now would leave
    fn settle(&self, store: &MyStore) { … }         // finish background work
    fn check(&self, store: &MyStore, just_opened: bool, label: &str) { … } // engine internals
}

proptest! { #[test] fn conforms(steps in conformance::steps()) { conformance::history(subject(), &steps) } }
#[test] fn keeps_the_contract() { conformance::contract(subject()) }
```

- `history`: random blocks over every table shape (a variable sequence, a fixed one, one in a
  sub-directory, a scoped map, a point-lookup map) driven through `Tiered` (§5) as an index
  drives it: applies, stages (a full batch finalized), finalizes through any held block,
  reorgs, reopens and power loss. After every step the view reads like an oracle of the held
  blocks over the finalized ones, the durable view like the finalized ones alone, the tips
  (`durable_tip`, `applied`, `staged`) like the oracle's, and `Tiered::check` holds; a crash or
  reopen keeps exactly what was finalized. Blocks a reorg or crash dropped are replaced with
  different bytes at the same positions and keys, so a stale held item cannot pass. Views pinned
  earlier are re-checked after later steps (structural sharing never leaks a later write);
  range limits on both sides of each answer's size. Swarm-tested: whole step kinds switched off
  per case (bulk alone, no reorgs, …).
- `contract`: an empty open, identity refused across kind, format and network, a reopen resuming
  at the tip, `verify` clean with every commit counted, store misuse (a tip that does not
  advance, changes for another schema) and every `Tiered` precondition (§5) panicking before any
  state moves, work continuing after each.
- `Model` doubles as the expected state for an engine's own crash and fault tests.

`DiskEngine` runs the suite on `SimFs` (power loss = `SimFs::power_loss`, settle = merges, check
= LSM tiers and files), then what only files can show: every crash state of a commit history,
every failed I/O call, every failed read at open, its invariant checks firing, open's trimming and
refusals, the manifest body's golden bytes, and verify (bad page, lost file, a file a merge retired
mid-scrub). The LSM's own tests cover its layout arithmetic, prefetch plans and filters.
`tiered.rs` fire-drills each `Tiered::check` invariant by breaking it by hand.

## 5. Tiering

Every index holds the blocks above its durable tip the same way, so the port's crate holds them
once, over any engine:

```rust
impl<S: Store> Tiered<S> {
    pub fn new(store: S, batch: NonZeroUsize) -> Self;
    pub fn apply(&mut self, changes: Changes);                     // tip block: RAM, reorgable
    pub fn stage(&mut self, changes: Changes, weight: usize) -> bool; // final block; true = batch full
    pub fn finalize(&mut self, through: Height);                   // held through `through`: one commit
    pub fn reorg(&mut self);                                       // every applied block dropped
    pub fn view(&self) -> TieredView<S::View>;                     // held first, then durable
    pub fn durable_tip(&self) / applied(&self) / staged(&self) -> Option<BlockRef>;
}
```

- **A block = one `Changes`**, tipped by that block and keyed exactly as the store holds it. The
  held tier is those blocks plus, per table, an `imbl` structure over their items (sequence
  records past the durable length, map rows by key): a view is O(tables) pointer copies, so a
  writer republishes per block.
- **`TieredView<V>`** is a `View`, and a `SequenceRead` / `MapRead` when `V` is. A position past
  the durable length reads the held records; a key reads the held rows first, and `values` asks
  durable once for the misses; `range` merges both runs (keys are unique across tiers) and keeps
  the over-`limit` = `None` rule. `durable()` is the committed view (the seam).
- **Staged and applied never coexist.** A final `Apply` arrives only when the producer's window is
  empty, and a tip block builds on durable: a writer finalizes its staged blocks first.
- **`stage`'s `weight`** is the source block's bytes, so `[index] batch_mib` still means MiB of
  decoded blocks per commit, whatever an index's rows weigh.
- **`finalize`** merges the held blocks through `through` into one `Changes` and commits it (one
  fsync). A failed commit panics naming the index and its directory: the store is poisoned and a
  restart recovers.
- **Restart and reorg leave the same state**: nothing held. An index with a carry re-reads it off
  the view's tip record (tree-state's frontiers), so the rare path is the boot path. A folded
  index (compact-block's tree sizes) carries nothing: its `fold` reads the parent tip record
  through a reader over the view ([nfs.md](nfs.md) §5).

Preconditions panic at the top of the call, naming the index, before any state moves:

| Call             | Panics on                                                                   |
| ---------------- | --------------------------------------------------------------------------- |
| `apply`          | staged blocks held                                                          |
| `stage`          | applied blocks held                                                         |
| `apply`, `stage` | changes for another schema; a height other than the next; a map key twice   |
| `finalize`       | `through` at or below durable, or above the last held; splitting the staged |
| `reorg`          | staged blocks held (final never rolls back)                                 |

An index writer is then its schema, its per-block encoding (`block → Changes`, a pure `fold` over a
reader of the held view where the index has one) and any carry it derives; its own `run` loop maps
the sink's steps (`docs/design/data-sink.md`) onto `Tiered`:

| Step              | Writer                                                                    |
| ----------------- | ------------------------------------------------------------------------- |
| `Apply` final     | at or below durable = replay, skipped; else encode, `stage`; full → `finalize` |
| `Apply`           | `finalize` the staged, encode, `apply`, publish                           |
| `Finalized { h }` | `finalize(h)`, publish the view, then the durable tip                     |
| `Reorg`           | `reorg`, re-read the carry, publish, mark the gate                        |
| `Shutdown`        | `finalize` the staged                                                     |
