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

## [0.4.0] - 2026-08-14

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.3.0] - 2026-08-04

### Added
- `PoolTypeError::DuplicatePoolType` variant.
### Changed
- **Breaking** — `ValidatedBlockRangeRequest` now stores its parsed `u32`
  block-height endpoints. Consumers read the heights directly and drop their
  own `as u32` casts at the call site.
- **Breaking** — pool-type validation now rejects a request that names the
  same pool more than once (returning `PoolTypeError::DuplicatePoolType`)
  instead of silently collapsing the duplicate into a single pool.
- Internal — the hand-written proto utility helpers were DRY'd and made
  expression-oriented (parse-don't-validate), and the build script was
  deduplicated. No effect on the generated wire types beyond the changes above.
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-07-13

### Added
- Pool-type filter serves Ironwood by default (`include_ironwood: true`), so
  clients that predate the field still receive `ironwoodActions` (unknown
  protobuf fields are carried harmlessly).
### Changed
- `RawTransaction.data` is generated as `bytes::Bytes` rather than `Vec<u8>`
  (prost `bytes` config, scoped to this one field). The wire format is unchanged;
  it lets the serving path hand the same transaction to many streaming clients as
  refcount bumps instead of a copy each.
- Lightwallet protocol vendored subtree updated to v0.5.0:
  `CompactTx.ironwoodActions` (field 9, `CompactOrchardAction`-shaped) and
  `CompactBlock.ironwoodCommitmentTreeSize`.
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
