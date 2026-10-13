---
default: minor
---

# `zainod`: `[index.compact_block]`, `[index.block_hash]`, `[index.tree_state]` and `[index.transparent_address]` each …

`[index.compact_block]`, `[index.block_hash]`, `[index.tree_state]` and `[index.transparent_address]` each take `enabled` (default true) and `path`; a disabled index's methods answer `UNIMPLEMENTED`, and at least one must stay enabled. Compact-block's fee index (`value_balance`, beside it) runs with it and fills every `CompactTx.fee`. `[sync]` holds `finalised_depth`, `concurrency` (32), `batch_mib` (64) and `queue_mib` (256), shared by every index.
