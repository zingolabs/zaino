---
default: minor
---

# `zaino-primitives`: `Transaction::fee(spent)`: the value left in the transparent pool (Σ prevouts − Σ outputs + each shielded …

`Transaction::fee(spent)`: the value left in the transparent pool (Σ prevouts − Σ outputs + each shielded pool's value balance), `FeeError::{Negative, Overflow}`; the value-balance index and the chain view's peer quorum share it. `PeerTxId` (txid + v5 auth digest: how peers name an unmined transaction) moves here from zaino-peers.
