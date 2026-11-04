---
default: minor
---

# `zainod`: `zaino_finalized_tip` gauge

`zaino_finalized_tip` is the height of the finalized boundary: the last block the follower hands
the index writers. `zaino_fetch_height` climbs to it during a sync, while `zaino_best_tip` stays
about 1,000 blocks above it.
