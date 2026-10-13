---
default: minor
---

# `zaino-snapshot`: `Snapshot::mempool()` and `mempool_stream()` also answer while a peer quorum reads the mempool …

`Snapshot::mempool()` and `mempool_stream()` also answer while a peer quorum reads the mempool (`ChainViewSnapshot::peers_quorate`) with no trusted validator holding the tip; G6 checks the gate against both. `mempool_transactions{state}` and `/statusz`'s mempool counts gain `quorum` (held by the peer quorum, no trusted listing), no longer counted as `ours_unverified`.
