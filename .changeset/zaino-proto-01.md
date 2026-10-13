---
default: major
---

# `zaino-proto`: Removed: the `heavy` (default) and `grpc_proxy_server` features (the crate no longer depends on `zebra-state` …

Removed: the `heavy` (default) and `grpc_proxy_server` features (the crate no longer depends on `zebra-state` or `zebra-chain`, nor re-exports `prost` / `tonic`), `proto::utils` (`PoolTypeFilter`, `pool_types_from_vector`, `ValidatedBlockRangeRequest`, `blockid_to_hashorheight`, …), `proto::indexed_tip` (`zaino.index.v1.IndexedTipService`, added in 0.10.1) and `proto::proposal`. `CompactTxStreamer` no longer declares `GetBlockNullifiers` or `Ping` (nor the `Duration` / `PingResponse` messages); `GetTaddressTxids` is marked deprecated.

**Migration:** Depend on `prost` / `tonic` directly; parse `poolTypes` in the serving crate.
