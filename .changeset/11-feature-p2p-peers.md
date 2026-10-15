---
default: minor
---

# `zainod`: `[p2p]` peers keep the tip and mempool serving through a validator outage

With `[p2p] enabled = true` (default off), zainod joins the Zcash p2p network through
zebra-network. It serves no peer (listener on loopback, no services advertised).

- **Headers and blocks:** peers' headers extend the tip once they pass every header rule against
  zainod's own chain (proof of work, difficulty, time, linkage). Blocks are fetched by hash and
  checked against their header.
- **Finality:** moves on peers' word once outbound peers in more than 5 netgroups carry the chain
  and its difficulty has not collapsed. Otherwise it waits for a trusted validator.
- **Mempool:** each outbound peer's mempool is polled every 5 s. A transaction more than 2/3 of
  them list, from more than 5 netgroups, is served once it passes Zaino's consensus checks against
  the served UTXO set (needs `index.compact_block`).
- **Submission:** `SendTransaction` relays through the peer set (see the submission entry).
- With every trusted validator down and a best block less than 24 target spacings old (30 minutes
  at 75 s), zainod keeps serving as `degraded` (`/readyz` `200`). A stale tip is `down`.

| Key | Default | |
| --- | --- | --- |
| `enabled` | `false` | |
| `peer_target` | 8 | outbound peers, at least 3 |
| `initial_peers` | `[]` | empty = the network's DNS seeders; regtest must list peers |
| `cache_dir` | `<cache dir>/zaino/peers` | peer address cache |
