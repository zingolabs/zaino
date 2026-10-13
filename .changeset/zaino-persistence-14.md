---
default: minor
---

# `zaino-persistence`: Barrier commits

Barrier commits. On ext4 (barriers on, no `journal_async_commit`, no DAX) and on single-device XFS (no DAX, no shared extents), a commit writes back each grown sequence file (`sync_file_range`) and flushes the device once before the manifest, instead of one fsync per file: tree-state's ~90 flushes per commit become 2. A probe decides once per store at open (`Fs::durability` → `Durability::{PerFile, Barrier}`) and needs positive evidence for each fact, so every other filesystem keeps one fsync per file. LSM segments are new files and are always fsynced. `Fs` gains `durability` and `FileHandle` gains `write_back`; `SimFs::flush_device` models a device-wide flush for crash tests.
