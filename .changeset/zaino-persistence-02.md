---
default: major
---

# `zaino-persistence`: Tables are `const` handles declared once per index: `SequenceTable::new(id, name, record)`, …

Tables are `const` handles declared once per index: `SequenceTable::new(id, name, record)`, `MapTable::new(id, name, key, value, scope)`, `Tables::new(sequences, maps)` (ids = positions, checked at compile time), `Schema::new(kind, format, network, tables)` (now `Copy`, built once at open; `with_sequence` / `with_map` / `sequence_ids` / `map_ids` removed). `BlockChanges::new` is private: a block's delta comes from `Store::changes(at)` or `Overlay::changes(at)`, filled through `changes.sequence(T).append(..)` / `changes.map(T).insert(..)` (`SequenceAppends`, `MapInserts`). Reads mirror writes: `view.sequence(T)` (`count`, `record`, `records`) and `view.map(T)` (`value`, `values`, `range`); `View::schema()`. A handle of another schema panics naming the table. `BlockChanges::block()` = the block a delta is for. On-disk layout unchanged.

**Migration:** Declare the index's tables as `const` handles + `pub const TABLES: Tables`, open with `Schema::new(kind, FORMAT, network, TABLES)`, open deltas with `store.changes(block.at())`, and read with `view.sequence(T)` / `view.map(T)` instead of passing a `SequenceId` / `MapId`.
