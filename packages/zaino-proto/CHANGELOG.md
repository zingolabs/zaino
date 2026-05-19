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
stable tag `v0.1.2-zr4` (April 2025).

### Added
- `ValidatedBlockRangeRequest` type that encapsulates validations of
  the `GetBlockRange` RPC request, paired with `GetBlockRangeError`.
- `utils` submodule handling `PoolType` conversions, including
  `PoolTypeError` (conversion errors between `i32` and known
  `PoolType` variants) and `PoolTypeFilter` (which pools a compact
  block should expose).
- `utils` conversion helpers:
  - `pool_types_from_vector`, `pool_types_into_i32_vec`
  - `compact_block_to_nullifiers`,
    `compact_block_with_pool_types`
- `utils::blockid_to_hashorheight`, migrated here from
  `zaino-state::utils`.
- `GetMempoolTxRequest` request type (replaces the on-wire `Exclude`
  parameter of `GetMempoolTx` — see Changed/Removed).
- `CompactTxIn` and `TxOut` types alongside the upstream
  compact-formats proto sync.
- Cargo features `heavy` (enabled by `default`; pulls in
  `zebra-state`, `zebra-chain`, and `which`) and `grpc_proxy_server`
  (re-exports `prost` and `tonic` for downstream proxy-server
  crates). Consumers that only need the generated wire types can
  disable default features.
- Proto schema sync with upstream `lightwalletd`'s canonical
  `compact_formats.proto` / `service.proto`. New on the wire:
  `PoolType` enum (`TRANSPARENT`, `SAPLING`, `ORCHARD`),
  `BlockRange.poolTypes` field, and the `GetTaddressTransactions` RPC
  (intended replacement for `GetTaddressTxids`).

### Changed
- **Breaking** — the generated
  `compact_tx_streamer_server::CompactTxStreamer` service trait gains
  a required `get_taddress_transactions` method (from the upstream
  proto sync). Downstream crates that implement the server trait
  directly must add this method.
- **Breaking** — `GetMempoolTx` RPC parameter type changed from
  `Exclude` to `GetMempoolTxRequest`. The new request type wraps the
  same shortened-txid exclude list while leaving room for forward
  evolution.
- Build wiring updated to `tonic-prost` / `tonic-prost-build` 0.14
  (`tonic-build` dropped). `zebra-state`, `zebra-chain`, and `which`
  are now optional and gated behind the `heavy` feature.

### Deprecated
- Proto-level `GetTaddressTxids` is deprecated in favor of
  `GetTaddressTransactions` (kept on the wire for compatibility).

### Removed
- `Exclude` message type — replaced by `GetMempoolTxRequest` (see
  Changed).
