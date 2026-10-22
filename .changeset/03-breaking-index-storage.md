---
default: major
---

# `zainod`: one directory per index, on a new storage engine

The 0.10 finalised-state database is not read or migrated. Each index lives in its own
directory, set by `[index.<name>] path`:

| Index | Default directory | Serves |
| --- | --- | --- |
| `compact_block` | `<cache dir>/zaino/indexes/compact_block` | `GetBlock`, `GetBlockRange` |
| `utxo_set` | beside `compact_block` | each `CompactTx.fee` (on and off with `compact_block`) |
| `block_hash` | `<cache dir>/zaino/indexes/block_hash` | requests by block hash |
| `tree_state` | `<cache dir>/zaino/indexes/tree_state` | `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` |
| `transparent_address` | `<cache dir>/zaino/indexes/transparent_address` | `GetAddressUtxos(Stream)`, `GetTaddressBalance(Stream)`, `GetTaddressTransactions` |

In the container image `<cache dir>/zaino` is `/var/lib/zaino`.

- Every index commits atomically through a manifest, and checks a CRC on every 4 KiB page when it
  first reads it. A crash or power loss leaves the last commit.
- An index directory opened for another network, another index or another format is refused.
- Blocks reach disk once they are `sync.finalised_depth` deep. Blocks above that are held in
  memory, so a restart refetches them.

**Migration:** delete the old `storage.database` directory. A first start syncs each enabled
index from the trusted validators, or fills empty ones from `[snapshot]`.
