---
default: major
---

# `zainod`: zainod is rebuilt as an indexer over its trusted validators' JSON-RPC

zainod is rebuilt as an indexer over its trusted validators' JSON-RPC. It trusts what they hold (zebra validated it): bulk sync reads every final block by height from them, each checked against its own header (merkle root, coinbase height) and linked to the block before it; the tip follows their headers, in memory, from their tip − depth. It keeps its own indexes (compact blocks with their fees, block hash, tree state, transparent address), holds the blocks above the indexes' durable tips in memory, and answers every gRPC request from one snapshot, served only while a trusted validator holds its tip. The Direct (ReadState) backend, the LMDB finalised-state database, the ephemeral passthrough mode and the block cache are gone.

**Migration:** The old `storage.database` directory is not read: delete it. A first start syncs each enabled index from the validators (or from `[snapshot]`) before `/readyz` passes. Indexes default to `<cache dir>/zaino/indexes/<name>` (`/app/data/indexes` in the container image).
