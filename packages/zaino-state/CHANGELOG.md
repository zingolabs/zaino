# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `gettxoutsetinfo` is now served indexer-side via Zaino's own UTXO-set
  accumulator:
  - `chain_index::types::db::metadata::FinalisedTxOutSetInfoAccumulator` —
    new singleton type tracking the finalised transparent UTXO set:
    `transactions`, `transaction_outputs`, `bytes_serialized`,
    `hash_serialized: [u8; 32]`, `total_zatoshis`. Maintained incrementally by
    block write / delete / migration paths.
  - `hash_serialized` is a Zaino-defined XOR-of-BLAKE2b-256 multiset commitment
    over the 65-byte canonical UTXO entry
    `prev_txid || vout || value || script_hash || script_type`, domain-tagged
    `b"ZcashTxOutSet___"`. It is order-independent and incrementally
    maintainable; not byte-equal to zcashd's `hash_serialized`.
    `bytes_serialized` equals `transaction_outputs * 65` by construction.
  - `chain_index::types::db::metadata::tx_out_set_entry_digest` and
    `is_unspendable_tx_out` helpers. NonStandard transparent outputs
    (OP_RETURN, oversized, anything that isn't P2PKH or P2SH) are excluded
    from every accumulator field — matches zcashd's `IsUnspendable()` view of
    the UTXO set.
  - `FinalisedTxOutSetInfoAccumulator::apply_added_output` /
    `apply_removed_output` per-output helpers and `AccumulatorDeltaError`.
  - `ChainIndex::get_tx_out_set_info` chain-level method folds the
    non-finalised state on top of the finalised accumulator and returns the
    full `GetTxOutSetInfoResponse`. Returns
    `GetTxOutSetInfoResponse::Empty` while the indexer is still syncing
    finalised state.
  - `DbReader::get_previous_output` — new read-only path through
    `BlockTransparentExt::get_previous_output`, used by the chain-level fold
    to resolve non-finalised spends against the finalised UTXO set.
  - `BlockTransparentExt::get_previous_output` trait method and V1
    implementation (formerly only available behind the
    `transparent_address_history_experimental` feature flag; now
    unconditionally available).
  - New finalised-state singleton table `tx_out_set_info_accumulator`
    (LMDB key `tx_out_set_info_accumulator_1_2_0`). See the finalised-state
    changelog for the schema entry.
  - `ChainIndexError::internal` constructor.
### Changed
- `FetchService` and `StateService` now serve `gettxoutsetinfo` through
  `ChainIndex` instead of forwarding to the backing validator. Response fields
  `transactions`, `txouts`, `total_amount`, `height` and `bestblock` agree
  with zcashd's RPC; `bytes_serialized` and `hash_serialized` follow Zaino's
  own deterministic spec.
### Deprecated
### Removed
### Fixed
- Finalised-state DB v1.2.0: added a reverse transaction-id index
  (`txid_location`) so previous-output resolution is an O(log n) point lookup
  instead of a full scan of the `txids` table. This fixes a near-quadratic
  slowdown that made the v1.1.0 -> v1.2.0 migration appear to hang on large
  caches and progressively slowed clean sync. The migration is now a re-entrant
  two-stage backfill (build `txid_location`, then `spent` + accumulator) with
  per-stage progress trackers and progress logging.
- Finalised-state DB v1.2.0: caches built by 0.4.0-alpha.1 (recorded at v1.2.0
  with an unbuilt `txid_location` index) are detected on open and self-heal by
  rolling back to v1.1.0 and rebuilding the indices in place (temporary shim,
  removed at 0.4.0).
- `write_block` no longer issues two redundant `env.sync(true)` calls per block;
  the durable `txn.commit()` already fsyncs, so crash safety is unchanged.
- Fixed a compile error in the `transparent_address_history_experimental`
  feature (an undefined `outpoint` in block validation) that had broken the
  feature build since the v1.2 spent-index refactor.

## [0.2.0] - 2026-05-19

### Added

#### New methods on the `ChainIndex` trait
- Transparent-address queries (#1065): `get_address_balance`,
  `get_address_deltas`, `get_address_txids`, `get_address_utxos`.
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
