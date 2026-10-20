---
default: major
---

# `zaino-source`: the validator poll's latency is `method="poll"`, not `batch`

`zaino_validator_rpc_duration_seconds` and `zaino_validator_rpc_failures_total` label a
JSON-RPC batch by its purpose: `poll` for the chain view's mixed poll (`getblockchaininfo` +
`getrawmempool` + `getblockhash`, plus the metadata calls), else the batch's one method. Queries
on `method="batch"` now match nothing.
