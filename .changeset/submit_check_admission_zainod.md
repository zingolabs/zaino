---
default: minor
---

# `zainod`: `SendTransaction` answers from a trusted `checkadmission`, then relays through the peer set

`SendTransaction` first asks one trusted validator holding the verified tip for `checkadmission` (Zaino's zebra fork: `sendrawtransaction`'s checks, the fee policy and mempool conflicts included, nothing admitted or relayed). A refusal reaches the wallet at once and nothing is relayed; an admissible transaction is answered `Ok` at once. Validators without `checkadmission` (zcashd, stock zebrad) answer `-32601`, which is taken as their answer: they stay healthy, and the send falls back to the previous flow.

With `[p2p]` on, each attempt is now zebra's own relay over the peer set's existing connections (to a third of the ready peers), replacing a fresh isolated connection per push, which a zebrad peer drops as a second connection from the same IP. Spread = a trusted validator never pushed to lists it, or a peer quorum admits it. After `max_attempts` unseen relays, one trusted validator relays it.
