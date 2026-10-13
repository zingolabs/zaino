---
default: minor
---

# `zainod`: Wallet-facing behaviour: `CompactTx.fee` is filled (0 for a coinbase); `GetBlockRange` with `start > end` …

Wallet-facing behaviour: `CompactTx.fee` is filled (0 for a coinbase); `GetBlockRange` with `start > end` streams in decreasing height order; `poolTypes` naming no pool is `INVALID_ARGUMENT`, and `GetMempoolTx` filters by `poolTypes` as `GetBlockRange` does; `GetLightdInfo` reports `lightwalletProtocolVersion` `v0.5.0` and the served snapshot height, answered from the chain view without a validator call; `GetMempoolStream` encodes each transaction once for every subscriber. `GetBlockRangeNullifiers` and `GetTaddressTxids` are served as deprecated aliases for pepper-sync.
