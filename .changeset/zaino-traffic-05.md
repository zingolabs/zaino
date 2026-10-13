---
default: minor
---

# `zaino-traffic`: `TrafficBalancer::blocks_at(heights, urgency)`: best-chain blocks by height, one batched request to a trusted …

`TrafficBalancer::blocks_at(heights, urgency)`: best-chain blocks by height, one batched request to a trusted member (peers answer by hash alone), in the same tip / bulk block classes as `block(hash, ..)`, rounds retried until one serves them all.
