---
default: minor
---

# `zainod`: With `[p2p]` on, the mempool keeps serving with every trusted validator down: each outbound peer's mempool is …

With `[p2p]` on, the mempool keeps serving with every trusted validator down: each outbound peer's mempool is polled every 5 s, and a transaction > 2/3 of them list, from > 5 netgroups, is served once Zaino's consensus checks pass against the served UTXO set (`index.compact_block` needed: without it zainod warns at boot and peers admit nothing). `SendTransaction` is answered once that quorum lists the transaction. The chain view is built once the upgrade schedule is known (regtest's: a validator's first poll).
