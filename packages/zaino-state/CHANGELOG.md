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

#### New methods on the `ChainIndex` trait
- Transparent-address queries (#1065): `get_address_balance`,
  `get_address_deltas`, `get_address_txids`, `get_address_utxos`.
- Chain-tip / fork accounting (#1092): `branch_len_to_active_chain`,
  `chain_tips_from_nonfinalized_snapshot`.
- Block lookups (#1000): `get_block_hash`,
  `get_indexed_block_by_hash`, `get_indexed_block_by_height`.
- Subtree-root reporting (#853): `get_subtree_roots`, `pool_string`.
- Non-finalised-state policy (#1012): `max_serviceable_height`.
- Sync diagnostics (#1031): `max_backoff_window`,
  `new_with_sync_timings`.
- Misc: `source_error` (#962).

#### New public types and modules
- `chain_index::types::block_context::BlockContext` (re-exported as
  `chain_index::types::BlockContext`) — packages height + hash into
  a single value (#1028).
- `chain_index::types::wire::WireBlockIdError` — error type for
  business↔gRPC `BlockId` conversions (#1028).
- `chain_index::non_finalized_state::ChainIndexSnapshot` — replaces
  `NonfinalizedBlockCacheSnapshot` (now `pub(crate)`) as the
  public snapshot type returned by
  `ChainIndex::snapshot_nonfinalized_state` (#1012).
- `chain_index::source::validator_connector::ValidatorConnector`
  exposed as a dedicated module (#1065).
- `backends::config::CommonBackendConfig` — shared payload between
  `StateServiceConfig` and `FetchServiceConfig`, including the new
  `indexer_version` field that threads the running binary's version
  through to `LightdInfo.version` (#1061).
- `DonationAddress` type (#1008).
- `ShieldedPool` enum (#853).
- `NamedAtomicStatus` — shared status primitive used by the new
  logging surface (#888).

### Changed

- **Breaking** — `pub trait ChainIndex` gains the methods listed
  above as required methods without default bodies. Downstream
  implementers of the trait must add all of them.
- **Breaking** — `ChainIndex::snapshot_nonfinalized_state` now
  returns `Future<Output = Result<Self::Snapshot, _>>` and the
  `Snapshot` associated type is now `ChainIndexSnapshot` on
  `NodeBackedChainIndexSubscriber`'s impl (#1012).

### Removed

- `chain_index::types::primitives::BestTip` — relocated and renamed
  to `chain_index::types::BlockIndex` (which was already public in
  0.1.0); the inner field `blockhash` is renamed to `hash` and the
  type gains `Eq` / `Hash` derives (#1028).
- `non_finalized_state::NonfinalizedBlockCacheSnapshot` is now
  `pub(crate)` and is no longer part of the public API; consumers
  should use `ChainIndexSnapshot` (#1012).

### Fixed

- `ChainIndexSnapshot::get_chainblock_by_hash` and
  `get_chainblock_by_height` now delegate to the underlying
  non-finalized snapshot instead of always returning `None` (#1089).
- Restart path no longer crashes early when the validator's readiness
  signal arrives before the indexer's status is observed (#962).

## [0.1.0] - 2026-03-26

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.

Contents include the `chain_index` architecture (the `ChainIndex`
trait, `NodeBackedChainIndex`, the `finalised_state` `DbV0` / `DbV1`
versioned on-disk format with `Migration` framework, the
non-finalized state), the `source::BlockchainSource` trait, the
`backends` pluggable backend layer, the `encoding` module with the
`ZainoVersionedSerde` framework and read/write helpers,
`validator_connector`, the `LightWalletService` abstraction, and the
gRPC service implementing the upstream `lightwalletd`
`CompactTxStreamer` surface including `GetTaddressTransactions`.
