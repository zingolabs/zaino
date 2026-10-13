---
default: major
---

# `zaino-persistence`: `BlockChanges::map(T)` returns `MapChanges` (was `MapInserts`), with `insert` and `remove`; …

`BlockChanges::map(T)` returns `MapChanges` (was `MapInserts`), with `insert` and `remove`; `BlockChanges::removes(T)` lists a delta's removals. `Uncommitted::value` / `rows` yield `Entry` (`Value(Bytes)` or `Removed`) instead of a value.

**Migration:** Rename `MapInserts` to `MapChanges`; an `Uncommitted` implementation returns `Entry::Value(..)` where it returned the value, `Entry::Removed` for a removal it holds.
