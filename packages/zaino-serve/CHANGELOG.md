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

## [0.9.0] - 2026-09-26
### Added
- Serve `IndexedTipService/SubscribeIndexedTips` on the gRPC server. A subscription first receives the current indexed tip, then each canonical tip change, including a same-height reorg. A slow client receives only the latest tip, and graceful shutdown ends open subscriptions.
- JSON-RPC serving metrics `zaino.jsonrpc.request_duration_seconds{method}` and `zaino.jsonrpc.errors_total{method,code}`; unregistered method names are labelled `unknown`.
### Changed
- `BlockDeltas::from_domain` and the `getblockchaininfo` pool-balance rendering are infallible, because the domain types already guarantee the range. The `DeltaAmountOutOfRange` and `PoolBalanceOutOfRange` errors are removed.
  _Migration:_ Remove handling for `DeltaAmountOutOfRange` and `PoolBalanceOutOfRange`, and drop the `?` on the now-infallible conversions.
- Removed `zaino.grpc.requests_total` (= `zaino.grpc.request_duration_seconds` `_count`); `zaino.grpc.errors_total{code}` now carries the status name (`NotFound`). Feature `prometheus` removed.
  _Migration:_ Use `zaino.grpc.request_duration_seconds_count` for request volume.
- dependency `zaino-consensus` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-primitives` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
- dependency `zaino-state` 0.9.0→0.10.0 crossed the requirement `^0.9.0`
### Fixed
- Confirmation counts in verbose blocks, verbose headers, block deltas and transaction outputs are checked: a value below -1, a block value of 0, or a count past `u32` is rejected instead of passed through.
- `getaddressdeltas` rejects an invalid address as invalid params instead of admitting it.
- `z_gettreestate` writes the Orchard and Ironwood `finalRoot` in display order. Only the Sapling root is byte-reversed for display, so both pools previously named a root no chain ever had.
- `getblockchaininfo` renders `chainSupply` with its value; every backend previously reported it as zero.
### Internal
- The block-header render test builds its chainwork from a compile-time constant.

## [0.8.0] - 2026-09-11

### Added
- `getaddressdeltas` on the JSON-RPC server, answered through the source
  port.
### Changed
- **Breaking** — builds on zaino-proto 0.6.0, whose `CompactBlock` no
  longer has `proto_version`.
### Deprecated
### Removed
- **Breaking** — the `zcashd_support` feature and the zcashd-shaped wire
  types it gated: `ZcashdPeerInfo`, `ServiceFlags` and `PeerStateStats`.
  Zaino no longer supports zcashd as a backing validator.
### Fixed

## [0.7.0] - 2026-08-28

### Added
### Changed
- `z_getsubtreesbyindex` accepts the pool name `"ironwood"`; previously the
  interface deliberately rejected it. `GetSubtreeRoots` serves the same pool
  over gRPC through `zaino-proto` 0.5.0's `ShieldedProtocol::Ironwood`.
### Deprecated
### Removed
### Fixed

## [0.6.0] - 2026-08-14

### Added
- `rpc::jsonrpc::wire` — **this crate now owns the served JSON schema**
  (ADR-0009). Serde structs carrying zcashd's exact field names, one module per
  response family (`address`, `address_deltas`, `address_queries`,
  `block_deltas`, `block_header`, `block_subsidy`, `blockchain_info`,
  `chain_tips`, `common`, `hashes`, `mining_info`, `misc`, `node_info`,
  `peer_info`, `subtrees`, `treestate`), each with a `from_domain` conversion
  and its own golden serialization tests.
- Interface asymmetries are now recorded and tested where they are served:
  `z_gettreestate`'s `finalRoot` is display-order, `z_getsubtreesbyindex`'s
  subtree roots are not.
### Changed
### Deprecated
### Removed
### Fixed

## [0.5.1] - 2026-08-04

### Changed
- Internal DRY refactor with no public API or behavior change: JSON-RPC
  error-object construction is deduplicated, and the error-source walk is
  expressed as an `unfold`.
### Deprecated
### Removed
### Fixed

## [0.5.0] - 2026-07-13

### Added
### Changed
- tonic's TLS provider feature switches from `tls-ring` to `tls-aws-lc`,
  following the workspace's aws-lc-rs preferred CryptoProvider (ADR-0006).
- **Breaking** — the JSON-RPC handlers now take domain types from
  `ZcashIndexer` and apply `from_domain` at the boundary, so the served
  response types are this crate's rather than `zaino-fetch`'s or
  `zebra-rpc`'s. Downstream implementors of `ZcashIndexerRpcServer` see new
  return types on `get_spent_info`, `get_tx_out`, `get_tx_out_set_info`,
  `get_chain_tips`, `get_block_deltas`, `get_peer_info`, `get_mining_info`,
  `get_block_subsidy`, `get_block_header` and `get_mempool_info`.
- `zcashd_support` gates the zcashd-shaped peer-info types in this crate's wire
  module and forwards nowhere — it is now the only place in the workspace the
  feature gates anything.
### Deprecated
### Removed
### Fixed
- **zcashd error-code recovery was silently inert.** The error-chain downcast
  matched `zaino_fetch`'s connector type, which the new source stack never
  constructs, so every validator error code reached the client as a generic
  internal error. It now matches `zaino_source::FetchError`'s
  `FailureMode::RpcError(i64)` (the validator's code) and
  `zaino_state::LegacyRpcError` (Zaino's own), and is tested directly rather
  than only from the far side.
- `getspentinfo` reports zcashd's own `-5` / `Unable to get spent info` for an
  output with no spend on record. Neither this crate nor its predecessor served
  that code: the old path consumed it into a typed error, destroying the
  `RpcError` the recovery walks for, and the first rewrite reported `-8`.
- `getblockchaininfo` no longer fails against zebra 6.0, which serialises the
  deferred-development-fund value pool as `lockbox` rather than `deferred`.

## [0.4.0] - 2026-07-02

### Changed
- Version-alignment bump for the 0.5.0 workspace release; no changes to this
  crate's own public API or behavior.

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
