---
default: major
---

# `zaino-persistence`: `BlockChanges::tip()` → `block()` (the block a delta is for, its tip only once applied); …

`BlockChanges::tip()` → `block()` (the block a delta is for, its tip only once applied); `BlockChanges::bytes()` removed (unused); `assert_next` / `assert_run` move to zaino-sync (the port no longer names the block type); `BlockChanges::schema()` is public. Batched point lookups (`MapRead::values`, 64 keys and up) plan and issue their readahead across the rayon pool instead of one thread.

**Migration:** `changes.tip()` → `changes.block()`; `out.assert_next(parent, block)` → `zaino_sync::assert_next(out, parent, block)`, `BlockChanges::assert_run(..)` → `zaino_sync::assert_run(..)`.
