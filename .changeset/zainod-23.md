---
default: major
---

# `zainod`: `[sync] batch_mib` is removed: each index's store is opened with its own fixed write buffer (`WRITE_BUFFER`, …

`[sync] batch_mib` is removed: each index's store is opened with its own fixed write buffer (`WRITE_BUFFER`, the buffer's real heap) and commits when it fills, after the block at the chain's final tip and at shutdown.

**Migration:** Delete `batch_mib` from `[sync]` (an unknown key fails the load).
