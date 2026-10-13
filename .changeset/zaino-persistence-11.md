---
default: minor
---

# `zaino-persistence`: Single deletes on map tables: a `MapTable` declaring `deletes()` takes `changes.map(T).remove(key)` beside …

Single deletes on map tables: a `MapTable` declaring `deletes()` takes `changes.map(T).remove(key)` beside `insert` (insert once, remove at most once, never re-insert; a key inserted or removed per block, never both). A removed key reads absent through every view, store, write buffer and `Overlay` alike, and a range's `limit` counts live rows only. On disk a removal is a tombstone row (flag byte per row of such a table only); a merge drops a value and its tombstone together, keeps a lone tombstone, and reports any other duplicate as `SegmentError::Contract`. New counter `zaino_lsm_merge_cancelled_total`. Other tables' layout is unchanged.
