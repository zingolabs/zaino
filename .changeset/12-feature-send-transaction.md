---
default: minor
---

# `zainod`: `SendTransaction` checks admission first, then relays until the transaction spreads

- zainod first asks one trusted validator holding the tip for `checkadmission` (Zaino's zebra
  fork: `sendrawtransaction`'s checks, fee policy and mempool conflicts, nothing admitted or
  relayed). A refusal reaches the wallet at once and nothing is relayed. An admissible
  transaction is answered `OK` at once.
- Validators without `checkadmission` (stock zebrad) answer method-not-found. They stay healthy
  and the wallet is answered at the first relay's acceptance.
- Each attempt relays through one random trusted validator, or with `[p2p]` on through a third
  of the ready peers. The transaction is relayed again when it is not seen spreading within
  `[submission] propagation_threshold_secs` (15), up to `max_attempts` (4) relays.
- Rejections answer as lightwalletd does: gRPC `OK` carrying the validator's own JSON-RPC error
  code and message (`-22` for unparseable bytes, `-25` for an expired transaction or the wrong
  consensus branch). zebrad's "already in mempool" and "already queued" are rejections, not
  `UNAVAILABLE`.
- A sent transaction is listed by `GetMempoolTx` and `GetMempoolStream` before it propagates.
