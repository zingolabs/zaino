# LSM deletes

Point deletes for map namespaces declared with `deletes`, so a map can hold a set that shrinks
(for example unspent outputs only, a fraction of every output ever created) and its lookups stay
within page cache.

## Contract: insert once, delete at most once

A map namespace declared with `deletes` promises, per key: inserted at most once, deleted at most
once, never after its delete, never re-inserted. A key's state only moves forward
(absent → present → deleted). This is RocksDB `SingleDelete` / Pebble `SINGLEDEL` / fjall
`remove_weak` semantics, and it is what makes everything below simple:

- the port checks every commit against it (`check_commit`): a `Put` of a present key is
  `KeyExists`, a `Delete` of an absent key `KeyAbsent`, a `Put` or `Delete` of a removed key
  `KeyRemoved`, a key twice in one batch `DuplicateInBatch`, a `Delete` in a map without `deletes`
  `DeleteNotAllowed`; each applies nothing
- a commit is atomic and a segment no manifest lists is removed at open, so no replayed write can
  duplicate a row (Pebble `db.go` "no duplication of writes")
- `KeyRemoved` is refused while the removal is recorded. A removal is forgotten only when a merge
  cancels it with its value (below); from then the key reads as never inserted, which the port
  allows

## Representation

- Row of a `deletes` map = `key ‖ value ‖ flag`, flag 0 = value, 1 = tombstone; a tombstone's
  value bytes are all zero (`lsm/layout.rs` `encode_row` / `decode_row`, golden bytes beside them).
  Any other flag, or a non-zero value under a tombstone = corruption (fjall `ValueType`, Pebble's
  empty SINGLEDEL value). Maps without `deletes` have no flag byte.
- Tombstone keys go into the segment's filter like any key (LevelDB, RocksDB, fjall, Pebble all
  do; a missing filter entry = the lookup skips the segment and the value comes back).
- A key inserted and removed while both are still in the write buffer is a cancelled pair. When
  the buffer is written, the value goes in that write's segment and the tombstone in a newer
  tombstone-only segment, so the removal stays recorded until a merge cancels the two. One segment
  never holds both rows of a key.

## Reads: a tombstone wins in any order

Under the contract a key has at most one value row and one tombstone row anywhere. Present ⇔ some
segment (or the buffer) holds its value and none holds its tombstone. So a read never depends on
segment order, and the non-adjacent merge policy (`merge_candidates`) stays:

- point lookup: every segment whose filter admits the key is checked (a map without `deletes`
  stops at the first hit); a tombstone anywhere = absent
- range scan: the cursor's k-way merge reads every row in range, and a key with a tombstone in any
  segment is dropped; chunk budgets count live rows only (fjall `range.rs`, Pebble `Iterator`)
- the write buffer above the segments: a buffered removal masks the durable value, and a
  cancelled pair reads as removed

## Merges: a pair cancels, a lone tombstone stays

- value + tombstone of one key, both inputs → both dropped (safe in any merge: no other row of
  that key exists anywhere)
- lone tombstone (its value in a segment outside the merge) → kept
- two values or two tombstones of one key, or three rows → `SegmentError::Contract` (Pebble's
  `NondeterministicSingleDelete` / `IneffectualSingleDelete` cases). It surfaces at the next
  write as `CommitError::Corrupted`, and writes are refused: no retry can merge those inputs
- outputs shrink: output rows + 2 × cancelled = input rows (checked); all cancelled = no output
  segment, the inputs retire with nothing listed in their place

Cancelled pairs are exported (`zaino.lsm.merge_cancelled_total`); a tombstone-triggered merge
(RocksDB `CompactOnDeletionCollector`) only if measured space says so.

## Tests

- conformance `history`: `scanned` declares `deletes`; insert-once / delete-once histories
  (removals drawn from held keys, committed or buffered, sometimes many per commit, so whole
  segments cancel) through commits, merges, reopens and power loss at fanouts 2 / 3 / 8; point
  reads and chunked scans in both directions checked after every step (LevelDB `Randomized`,
  RocksDB `db_stress` expected state, Pebble metamorphic key manager)
- conformance `contract`: removed twice, re-inserted after a removal (buffered, then flushed),
  inserted and removed in one batch, removed from a map without `deletes` → refused
- adapter: a removal is forgotten only when a merge cancels its pair (refused while buffered,
  written and reopened; accepted once cancelled)
- merge stream table (`lsm/tests.rs`): pair cancels in either order, lone tombstone kept, two
  values / two tombstones / three rows = `Contract`, all-cancelled merge = no output
- resurrection: value, tombstone-only segment and an unrelated one, in every list order and after
  each pairwise merge → absent by lookup and cursor
- encoding: golden bytes; unknown flag / non-zero tombstone value rejected
- crash: every crash state of commits + a cancelling merge → each namespace = the model at an
  acknowledged or the attempted commit, a removed row never back
