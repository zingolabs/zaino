---
default: minor
---

# `zaino-chainview`: Peer-quorum mempool: `ChainView::peer_mempool(utxos)` returns the `PeerMempool` to spawn

Peer-quorum mempool: `ChainView::peer_mempool(utxos)` returns the `PeerMempool` to spawn. Every 5 s it asks each live outbound peer for its whole mempool (`ValidatorP2pSource::mempool`, that peer alone answering) and keeps each listing 15 s. A transaction is admitted when > 2/3 of the counted listings hold its exact id, from > 5 netgroups, and our consensus checks pass (bytes by id from the peers via `transactions`; expiry and v5+ branch against the block above the verified best; transparent prevouts unspent in `UnspentOutputs` or a held transaction's outputs; fee ≥ 0, served as the entry's fee). It stays while > 1/3 list it. `mempool()` also opens when the listings span > 5 netgroups (`ChainViewSnapshot::peers_quorate`), so the mempool and `SendTransaction` (accepted once the quorum lists it) work with every trusted validator down. `spread().peers` counts the peers listing it, `spread().quorum` = the quorum holds it. Every held transaction is served (a trusted listing, the quorum, or ours): `Spread.servable` and `ChainViewSnapshot::fixed`'s `unlisted` are gone. Transaction `inv`s are no longer read.
