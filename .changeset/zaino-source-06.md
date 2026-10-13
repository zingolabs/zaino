---
default: major
---

# `zaino-source`: Two `ChainDataSource` implementations

Two `ChainDataSource` implementations. `ZebraRpcAdapter` is renamed `ZcashLegacyRpcClient` (JSON-RPC for everything). The new `ZcashGrpcClient::at(legacy, indexer_address, timeouts, link)` fetches blocks over zebrad's indexer gRPC (`GetBlock`, and `GetBlockRange` per consecutive run of heights) as stored consensus bytes, no hex or JSON, multiplexed over `ceil(max_connections / 20)` HTTP/2 connections, and asks the rest through `legacy`. JSON-RPC blocks: the body buffer is sized from `Content-Length`, and the hex is decoded by `const-hex` (SIMD) straight from the string's bytes (no UTF-8 pass). `FailureMode::GrpcStatus(code)`; `FailureMode` is `Copy`.

**Migration:** `ZebraRpcAdapter` → `ZcashLegacyRpcClient` (same `at`). A `match` on `FailureMode` gains `GrpcStatus(_)`.
