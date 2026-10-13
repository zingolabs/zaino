---
default: major
---

# `zaino-persistence`: `DiskEngine::new(fs, LsmConfig { fanout, merge_slots })`: the map tables' merge fanout (default 16) and the …

`DiskEngine::new(fs, LsmConfig { fanout, merge_slots })`: the map tables' merge fanout (default 16) and the merges doing I/O at once (default 4), the latter shared by every store one engine opens (was process-wide).

**Migration:** `DiskEngine::new(fs)` → `DiskEngine::new(fs, LsmConfig::default())`.
