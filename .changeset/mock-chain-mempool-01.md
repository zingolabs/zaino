---
default: minor
---

# `zaino-primitives`: Feature `testing`: `MockChain` holds a mempool and exposes its `Ledger`

Feature `testing`: `MockChain::submit((tx, bytes))`, `mempool()`, `evict(txid)` and `mine_mempool()`: the network's unmined transactions, each admitted on the best chain by the same `Ledger` mining uses and re-admitted whenever the best moves (transactions a reorg unmines come back; mined, conflicting or orphaned ones leave). `MockChain::ledger(tip)` returns that `Ledger` (`admit`, `readmit`, `unspent`), refusing with `Refused`; mempool entries are `Unmined { tx, bytes, fee }`. `MockChain::ancestor(tip, height)` and `fork_point(a, b)` walk any tip's path.
