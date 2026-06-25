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

## [0.3.0] - 2026-06-17

### Added
- JSON-RPC service gains `get_tx_out_set_info`, `get_chain_tips`, `get_tx_out`,
  and `get_spent_info` handlers, each delegating to the corresponding
  `zaino_fetch::JsonRpSeeConnector` method.
- `grpc_routes` — assembles the tonic router for the gRPC service, split out of
  server spawn so the routes can be built independently of binding a listener.
### Changed
- **Breaking** — the JSON-RPC `#[rpc(server)]` trait gains four required methods
  (`get_tx_out_set_info`, `get_chain_tips`, `get_tx_out`, `get_spent_info`) with
  no default body; downstream implementors of the trait must add them.
- **Breaking** — `Server::spawn` no longer takes the
  `<Indexer: ZcashIndexer + LightWalletIndexer>` type parameters (they moved to
  `grpc_routes`) and now binds its `TcpIncoming` listener internally.
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-05-19

### Added

- `z_validateaddress` handler on `pub trait ZcashIndexerRpc`,
  delegating to `zaino_fetch::JsonRpSeeConnector::z_validate_address`
  (#389). Shipped pre-deprecated; logs
  `zaino_fetch::jsonrpsee::response::z_validate_address::DEPRECATION_NOTICE`
  on every call.

### Changed

- **Breaking** — `pub trait ZcashIndexerRpc` (annotated with
  `#[rpc(server)]`) gains a required `z_validate_address` method
  without a default body. Downstream crates that implement the trait
  directly must add this method.

## [0.1.0] - 2026-03-26

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.

Contents include the `rpc::jsonrpc` module with the `ZcashIndexerRpc`
trait (22 zcashd-compatible methods at the time of publish),
`server::jsonrpc::JsonRpcServer` / `JsonRpcServerConfig` /
`JsonRpcClient`, and the `server::config::GrpcServerConfig` /
`server::grpc::GrpcTls` gRPC configuration types.
