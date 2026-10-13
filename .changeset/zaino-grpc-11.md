---
default: minor
---

# `zaino-grpc`: A request naming a height (`GetBlock`, `GetBlockRange`, `GetTreeState`, by height or hash) answers up to its …

A request naming a height (`GetBlock`, `GetBlockRange`, `GetTreeState`, by height or hash) answers up to its own index's durable tip, which during sync may be above the served tip; `GetLatestBlock`, `GetLatestTreeState`, `GetSubtreeRoots`, `GetLightdInfo.blockHeight` and the address methods stay at the served tip (the lowest durable tip, held by every enabled index). `GetBlockRange`'s `OUT_OF_RANGE` now names the compact-block tip.
