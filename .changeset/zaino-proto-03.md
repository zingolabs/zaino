---
default: minor
---

# `zaino-proto`: The vendored zebrad indexer proto gains `GetBlockRange`

The vendored zebrad indexer proto gains `GetBlockRange(BlockRangeRequest { start, end }) returns (stream BlockAndHash)` (elicbarbieri/zebra `perf/indexer-block-range`); `BlockAndHash.data` is `Bytes` (a block = a slice of the received frame, never copied out).
