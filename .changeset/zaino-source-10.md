---
default: major
---

# `zaino-source`: `ResponseSizeCap` bounds every JSON-RPC reply, and batches split to fit it

`LinkLimits::response_size_cap` (a `ResponseSizeCap`: one full block's reply ..= 32 MiB) replaces the fixed 32 MiB reply cap. `get_blocks_by_height` and `get_raw_mempool_transactions` split their batches so no reply can exceed it, whatever the caller asks for.
