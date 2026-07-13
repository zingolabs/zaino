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

## [0.1.3] - 2026-06-18

### Changed
- Version bump from 0.1.2 to 0.1.3: the 0.1.2 slot on crates.io was
  consumed by a premature publish in August 2025 and subsequently yanked.

## [0.1.2] - 2026-06-17

### Fixed
- `compact_block_to_nullifiers` now also clears each transaction's `vin` and
  `vout`, so the nullifiers-only compact block no longer leaks transparent
  inputs/outputs — restoring lightwalletd compact-block parity (#1067).

## [0.1.1] - 2026-05-19

### Added
- Cargo feature `heavy` (enabled by `default`) — gates the optional
  `zebra-state`, `zebra-chain`, and `which` dependencies behind a
  feature flag so consumers that only need the generated wire types
  can disable default features.
- Cargo feature `grpc_proxy_server` — when enabled, re-exports `prost`
  and `tonic` from the crate root so downstream proxy-server crates
  can depend on a single zaino-proto version of those dependencies.
- Build wiring updated to `tonic-prost` / `tonic-prost-build` 0.14
  (`tonic-build` dropped).

## [0.1.0] - 2026-03-25

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.

Contents include the generated `compact_tx_streamer_server::CompactTxStreamer`
service trait (with `GetTaddressTransactions`), the `utils` module
(`PoolType` conversion helpers, `PoolTypeError`, `PoolTypeFilter`,
`blockid_to_hashorheight`), `ValidatedBlockRangeRequest`,
`GetMempoolTxRequest`, and the proto schema synced with upstream
`lightwalletd` (`PoolType` enum, `BlockRange.poolTypes`).
