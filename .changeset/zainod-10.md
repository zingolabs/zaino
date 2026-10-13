---
default: minor
---

# `zainod`: `SendTransaction` submits to one random entry per attempt and resubmits through another when the transaction …

`SendTransaction` submits to one random entry per attempt and resubmits through another when the transaction is not seen spreading within `[submission] propagation_threshold_secs` (15), up to `max_attempts` (4). `[p2p]` (off by default: `enabled`, `peer_target` (8), `initial_peers`, `cache_dir`) joins the Zcash p2p network, advertising no services (Zaino serves no peer): peers become submission entries before one trusted validator's verdict, their mempool listings admit transactions (below), and they serve headers and blocks (below).
