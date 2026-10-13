---
default: minor
---

# `zainod`: `[sync.lsm_store]`: `fanout` (16, same-tier segments merged into one; below 2 refused) and `merge_slots` (4, …

`[sync.lsm_store]`: `fanout` (16, same-tier segments merged into one; below 2 refused) and `merge_slots` (4, merges doing disk I/O at once across every index), `merge_mib_per_sec` (200, read + written MiB/s those merges share).
