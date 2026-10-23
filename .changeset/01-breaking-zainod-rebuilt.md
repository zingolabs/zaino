---
default: major
---

# `zainod`: rebuilt as a lightwalletd-compatible indexer over trusted validators

zainod 0.11 is a new daemon, not an upgrade of 0.10. It serves the lightwalletd `CompactTxStreamer`
gRPC from its own indexes, and reads the chain from one or more trusted Zebra validators.

- **Chain source:** each `[[trusted_validators]]` entry's JSON-RPC (optionally its indexer gRPC).
  zainod trusts what they hold: every block is checked against its header (merkle root, coinbase
  height) and linked to its parent, and the tip follows the most-work header any trusted validator
  serves.
- **Indexes:** compact blocks (each `CompactTx.fee` filled), block hash → height, commitment-tree
  state (Sapling, Orchard, Ironwood) and transparent addresses, each in its own directory. Blocks
  above the indexes' durable tips are held in memory and served, so reorgs never touch disk.
- **Serving:** every request reads one consistent snapshot of the chain and the indexes. gRPC is
  served only while a trusted validator holds the tip (or, with `[p2p]` on, while peers carry a
  fresh tip).
- **Gone:** the JSON-RPC server, the `state` / `direct` backend (Zebra `ReadStateService`,
  `zebra_db_path`), the LMDB finalised-state database and its migrations, the ephemeral
  finalised-state mode and the block cache.

**Migration**

- Rewrite the config (see the config entry) and delete the old `storage.database` directory: it
  is not read.
- The first start builds every enabled index from the trusted validators, or downloads them with
  `[snapshot]`, before `/readyz` passes. Plan for the initial sync.
- Run zainod under a service manager that restarts it: any panic aborts the process and the first
  long-running task to stop ends it (no in-process restart).
