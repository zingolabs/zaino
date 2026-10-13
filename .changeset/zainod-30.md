---
default: minor
---

# `zainod`: `validator_client = "legacy_rpc" | "grpc"` (default `legacy_rpc`): `grpc` fetches blocks over each …

`validator_client = "legacy_rpc" | "grpc"` (default `legacy_rpc`): `grpc` fetches blocks over each validator's zebrad indexer gRPC (`indexer_address`, required for every validator; zebrad with `GetBlockRange`) as stored bytes, no hex or JSON, and everything else over JSON-RPC. The `hex` crate is replaced by `const-hex` (SIMD) workspace-wide.
