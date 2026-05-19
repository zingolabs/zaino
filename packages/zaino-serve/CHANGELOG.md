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

### Added

Four new JSON-RPC handlers on `pub trait ZcashIndexerRpc`, each
delegating to the matching `JsonRpSeeConnector` method in
`zaino-fetch` 0.1.1:

- `z_validateaddress` (#389) — shipped pre-deprecated; logs the
  `zaino_fetch::jsonrpsee::response::z_validate_address::DEPRECATION_NOTICE`
  on every call.
- `gettxout` (#1085).
- `getchaintips` (#1092).
- `getspentinfo` (#1093).

### Changed

- **Breaking** — `pub trait ZcashIndexerRpc` (annotated with
  `#[rpc(server)]`) gains four required methods without default
  bodies, listed above. Downstream crates that implement the trait
  directly must add `z_validate_address`, `get_tx_out`,
  `get_chain_tips`, and `get_spent_info`.

## [0.1.0] - 2026-03-26

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.

Contents include the `rpc::jsonrpc` module with the `ZcashIndexerRpc`
trait (22 zcashd-compatible methods at the time of publish),
`server::jsonrpc::JsonRpcServer` / `JsonRpcServerConfig` /
`JsonRpcClient`, and the `server::config::GrpcServerConfig` /
`server::grpc::GrpcTls` gRPC configuration types.
