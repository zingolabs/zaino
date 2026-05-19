# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-05-19

This release covers all main-branch development since the previous
stable tag `v0.1.2-zr4` (April 2025). The headline addition is the
JSON-RPC server stack (`rpc::jsonrpc` + `server::jsonrpc`), which
did not exist at the baseline.

### Added

#### JSON-RPC server stack (#297 and follow-ups)
- `rpc::jsonrpc` module exposing the `pub trait ZcashIndexerRpc`
  (annotated with `#[rpc(server)]` from `jsonrpsee`).
- `server::jsonrpc::JsonRpcServer`, `JsonRpcServerConfig`,
  `JsonRpcClient`.
- 26 JSON-RPC passthrough methods on `ZcashIndexerRpc`, mirroring
  zcashd's surface: `getinfo`, `getblockchaininfo`, `getblockcount`,
  `getbestblockhash`, `getblock`, `getblockheader`, `getblockdeltas`,
  `getblocksubsidy`, `getchaintips`, `getdifficulty`, `gettxout`,
  `getspentinfo`, `getrawtransaction`, `sendrawtransaction`,
  `getrawmempool`, `getmempoolinfo`, `getaddressbalance`,
  `getaddressutxos`, `getaddresstxids`, `validateaddress`,
  `z_validateaddress`, `getpeerinfo`, `getmininginfo`,
  `getnetworksolps`, `z_gettreestate`, `z_getsubtreesbyindex`.
  Each handler delegates to the corresponding `JsonRpSeeConnector`
  method in `zaino-fetch`; per-method PR attribution lives in
  `zaino-fetch/CHANGELOG.md` under 0.2.0.

#### gRPC server
- `server::config::GrpcServerConfig` and `server::grpc::GrpcTls`
  expose TLS configuration on the gRPC service (replaces the simpler
  `GrpcConfig` — see Removed).
- Generated `CompactTxStreamer` server impl gains
  `get_taddress_transactions` to match the upstream `lightwalletd`
  proto sync (driven by `zaino-proto` 0.2.0).
- `z_validate_address` handler on `ZcashIndexerRpc` is shipped
  pre-deprecated; logs `DEPRECATION_NOTICE` from `zaino-fetch` on
  every call (#389).

### Changed

- **Breaking** — `pub trait ZcashIndexerRpc` accumulates new required
  methods over the 0.2.0 cycle. Each merge adds a method without a
  default body; downstream implementers of the trait must implement
  every method or the build fails. (Trait introduction in #297; the
  full method list is enumerated above.)
- **Breaking** — `server::config::GrpcConfig` is renamed to
  `GrpcServerConfig` and grows TLS options (#571).
- `server::jsonrpc::JsonRpcServer::spawn` extracts its inline
  shutdown-polling closure into a private `shutdown_signal` async fn
  (#1054, parent #1051). Pure refactor; no API change at the call
  site.

### Removed

- `server::config::GrpcConfig` — renamed to `GrpcServerConfig`
  (see Changed).
