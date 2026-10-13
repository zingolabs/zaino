---
default: major
---

# `zainod`: Zaino serves no JSON-RPC: the JSON-RPC server, its cookie auth and port 8237 are gone

Zaino serves no JSON-RPC: the JSON-RPC server, its cookie auth and port 8237 are gone. The gRPC service no longer offers `zaino.index.v1.IndexedTipService` (added in 0.10.1), `GetBlockNullifiers` or `Ping`; each answers `UNIMPLEMENTED`.

**Migration:** Send node queries to the validator's own JSON-RPC. Replace `GetBlockNullifiers` with `GetBlockRange` and `poolTypes`, and `IndexedTipService` with polling `GetLatestBlock`.
