---
default: minor
---

# `zaino-persistence`: Map segments written by commits and merges, and sequence appends, leave page cache once synced …

Map segments written by commits and merges, and sequence appends, leave page cache once synced (`FileHandle::drop_cache`, `POSIX_FADV_DONTNEED`), unless the table declares `MapTable::cache_writes()` / `SequenceTable::cache_writes()` (a fold reads it): bulk writes no longer evict the tables folds read.
