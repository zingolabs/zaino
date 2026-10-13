---
default: patch
---

# `zainod`: Sync progress logs name the phase still outstanding: `Syncing blocks` (final blocks to the indexes; `height` …

Sync progress logs name the phase still outstanding: `Syncing blocks` (final blocks to the indexes; `height` = handed, else the lowest durable index tip, `—` when neither exists, never `0`), then `Fetching non-finalized blocks` (`fetched` / `total` above the final tip, `bps`, `eta`) while the NFS fills the window to best. `Block fetch stalled` / `Non-finalized block fetch stalled` only once that phase has not moved for 30 s, so a node whose indexes sit at the final tip no longer warns `height=0` every 30 s. `/statusz` `progress.rate` = the same phase's blocks/s. The per-index `Syncing` lines only while the indexes trail the final tip.
