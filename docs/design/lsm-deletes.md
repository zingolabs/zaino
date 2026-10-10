# LSM deletes (value-balance = unspent outputs only)

Point deletes for map tables, so value-balance's `outputs` holds the UTXO set (~14% of all
transparent outputs at height 1.75M: 21.5M of 155M rows, ~1 GB instead of 7 GB) and its fold's
prevout lookups fit in page cache.

## Contract: insert once, delete at most once

A map table declaring `MapTable::deletes()` promises, per key: inserted at most once, deleted at
most once, never after its delete, never re-inserted. A key's state only moves forward
(absent → present → deleted). This is RocksDB `SingleDelete` / Pebble `SINGLEDEL` / fjall
`remove_weak` semantics, and it is what makes everything below simple:

- value-balance's outpoints are unique (consensus); its fold removes only prevouts its lookup just
  found, and drops each from the run's unspent set as it spends it (a missing or already-spent
  prevout = `FoldError::MissingPrevout`, never a remove)
- a block is applied once: held heights are skipped (`zaino_sync::held`), a commit is atomic, a
  segment no manifest lists is removed at open (no replayed write can duplicate a row — Pebble
  `db.go` "no duplication of writes")
- violations the engine can see are refused where they are written (below)

## Representation

- Row of a `deletes()` table = `key ‖ value ‖ flag`, flag 0 = value, 1 = tombstone; a tombstone's
  value bytes are all zero (`lsm/layout.rs` `encode_row` / `decode_row`, golden bytes beside them).
  Any other flag, or a non-zero value under a tombstone = corruption (fjall `ValueType`, Pebble's
  empty SINGLEDEL value). Tables without `deletes()` keep their layout (no format change for any
  other index).
- Tombstone keys go into the segment's filter like any key (LevelDB, RocksDB, fjall, Pebble all
  do; a missing filter entry = the lookup skips the segment and the value comes back).
- `BlockChanges`: `changes.map(T).remove(key)` beside `insert`; per block, a key is inserted or
  removed, never both (a same-block create-and-spend emits neither). `remove` on a table without
  `deletes()` panics.

## Reads: a tombstone wins in any order

Under the contract a key has at most one value row and one tombstone row anywhere. Present ⇔ some
layer or segment holds its value and none holds its tombstone. So a read never depends on segment
order, and the non-adjacent merge policy (`merge_candidates`) stays:

- point lookup: every segment whose filter admits the key is checked (a table without `deletes()`
  stops at the first hit); a tombstone anywhere = absent
- range scan: every row in range read, a key with a tombstone in any segment dropped, the limit
  counted on live rows only (fjall `range.rs`, Pebble `Iterator`)
- above durable (write buffer, NFS overlay): a removal masks durable. Each overlay entry is owned
  by the newest block that wrote it, and `rebase` drops only entries owned by a block durable now
  holds (else rebasing past an insert would drop a later block's removal and the value would come
  back). A range asks durable for `limit` + the removals above it.

## Merges: a pair cancels, a lone tombstone stays

- value + tombstone of one key, both inputs → both dropped (safe in any merge: no other row of
  that key exists anywhere)
- lone tombstone (its value in a segment outside the merge) → kept
- two values or two tombstones of one key, or three rows → `SegmentError::Contract` (Pebble's
  `NondeterministicSingleDelete` / `IneffectualSingleDelete` cases), surfaced at the next commit
- outputs shrink: output rows + 2 × cancelled = input rows (asserted); all cancelled = no output
  segment, the inputs retire with nothing listed in their place
- write buffer: a key inserted in the buffer and removed by a later buffered block cancels there
  (never reaches a segment); one segment never holds both rows of a key

Cancelled pairs are exported (`zaino_lsm_merge_cancelled_total`); a tombstone-triggered merge
(RocksDB `CompactOnDeletionCollector`) only if measured space says so.

## value-balance

- fold: per block, per tx in chain order — each input's prevout spent out of the run's unspent set
  and removed, then the tx's outputs in; an output created and spent in one block = neither row; a
  second spend or a spend of a later tx's output = `MissingPrevout`. Prevouts from before the run:
  one batched probe, as before.
- fees stored: `fees` (a sequence, record h = block h's fees) written in the same block's changes.
  A held (resent) block reads its fees back, never re-folds: its prevouts may be removed by later
  blocks (Zebra `block_info`, Core undo data, scaled down to what the sink needs). Missing or
  malformed record = named error (`StoredFeesMissing` / `StoredFeesUnreadable`). The NFS reads the
  same record when value-balance is already durable past the block.
- on-disk format: stores built before this hold spent rows and no `fees` → rebuilt (resync);
  `FORMAT` stays 1

## Tests

- conformance `history` model: `scanned` declares `deletes()`; insert-once / delete-once histories
  (removals drawn from held keys: committed, buffered, in a node layer, across a reorg; sometimes
  hundreds per block, so whole segments cancel) through commits, merges, reopens and power loss at
  fanouts 2 / 3 / 8; point reads, batched reads of every removed key and scans at random limits
  checked after every step (LevelDB `Randomized`, RocksDB `db_stress` expected state, Pebble
  metamorphic key manager). The model is the oracle for every fanout, so no separate
  configuration-comparison test.
- conformance `contract`: a removal cancelled in the buffer, one over a committed row; removed
  twice / inserted and removed in one block / removed again after the buffer or a layer removed it
  / removed from a table without `deletes()` → refused
- overlay drill: an entry owned by an older block than its newest writer is caught by `check`
- merge stream table (`lsm/tests.rs`): pair cancels in either order, lone tombstone kept, two
  values / two tombstones / three rows = `Contract`, all-cancelled merge = no output
- resurrection: value, tombstone-only segment and an unrelated one, in three list orders and after
  each pairwise merge → absent by `get`, `get_many` and `range`, limit counting live rows
- encoding: golden bytes; unknown flag / non-zero tombstone value rejected
- crash: every crash state of commits + a cancelling merge → each table = the model at an acked or
  the attempted commit, a removed row never back
- value-balance: spend in the same block / run / across a commit; a prevout spent twice in one tx
  or again later in the run = `MissingPrevout`; resent blocks republish stored fees identically;
  missing / mismatched fees record = named error
- heavy `zaino-persistence` proptest loop (CLAUDE.md)
