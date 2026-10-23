---
default: minor
---

# `zainod`: per-index `enabled` and `path`; `[sync]` budgets

- `[index.compact_block]`, `[index.block_hash]`, `[index.tree_state]` and
  `[index.transparent_address]` each take `enabled` (default `true`) and `path`. A disabled
  index is never built and its methods answer `UNIMPLEMENTED`. At least one must stay enabled.
- `[sync]`, shared by every index:

| Key | Default | Meaning |
| --- | --- | --- |
| `finalised_depth` | 1000 | blocks below the tip kept reorg-able in memory; at least 1000 off regtest |
| `concurrency` | 32 | block fetches in flight ahead of the next one needed |
| `queue_mib` | 256 | finalized blocks one index may trail the fetch by before the fetch waits |

- `[sync.lsm_store]` tunes the storage engine's background merges: `fanout` (16, at least 2),
  `merge_slots` (4, merges doing disk I/O at once across every index) and `merge_mib_per_sec`
  (200, read + write bandwidth those merges share; commits are never paced). A `fanout` an
  enabled index cannot hold is refused at startup.
