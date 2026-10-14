---
default: major
---

# `zaino-persistence`: `MapTable::point` / `MapTable::scoped` replace `MapTable::new`; record counts are `len`

A map table is declared `MapTable::point(id, name, key, value)` (whole-key lookups) or `MapTable::scoped(id, name, key, value, scope)` (range reads sharing `scope` leading key bytes; a zero scope fails at compile time), replacing `MapTable::new(.., scope)` and its bare `0`. A sequence's record count has one name: `SequenceView::count` is now `SequenceView::len` (with `is_empty`), and `Uncommitted::record_count` is now `Uncommitted::len`, matching `SequenceRead::len`. `disk_bytes` is removed (zainod keeps its own directory walk).
