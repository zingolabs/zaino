# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `rpc::grpc::service.rs`, `backends::fetch::get_taddress_transactions`:
    - these functions implement the GetTaddressTransactions GRPC method of
      lightclient-protocol v0.4.0 which replaces `GetTaddressTxids`
- `chain_index`
  - `::finalised_state::db::v0::get_compact_block_stream`
  - `::finalised_state::db::v1::get_compact_block_stream`
  - `::types::db::legacy`:
    - `compact_vin`
    - `compact_vout`
    - `to_compact`: returns a compactTx from TxInCompact
  - new type: `non_finalized_state::ChainIndexSnapshot`
  - `NonFinalizedSnapshot` trait has new method: `max_serviceable_height`
  - `::types`
    - new submodule `primitives` with type `BlockIndex { height, hash }`
      (re-exported as `chain_index::types::BlockIndex`)
    - new submodule `block_context` with type
      `BlockContext { index, parent_hash, chainwork }`, constructor
      `BlockContext::new`, and accessors `hash`/`parent_hash`/`chainwork`/`height`
      (re-exported as `chain_index::types::BlockContext`)
    - new submodule `wire` carrying the business↔gRPC conversions:
      - `BlockIndex::to_wire()` → `proto::BlockId`
      - `BlockIndex::try_from_wire(proto::BlockId) -> Result<Self, WireBlockIdError>`
      - new error enum `WireBlockIdError` (`HashWrongLength`, `HeightOverflow`)
- `local_cache::compact_block_with_pool_types`
- `source::BlockchainSource` and implementors now expose transparent-address
  methods:
  - `get_address_deltas`
  - `get_address_balance`
  - `get_address_txids`
  - `get_address_utxos`
- `ChainIndex` and `NodeBackedChainIndexSubscriber` now expose transparent-address
  query methods:
  - `get_address_deltas`
  - `get_address_balance`
  - `get_address_txids`
  - `get_address_utxos`
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
- `get_mempool_tx` now takes `GetMempoolTxRequest` as parameter
- `chain_index::finalised_state`
  - `::db`
    - `::v0`
      - `get_compact_block` now takes a `PoolTypeFilter` parameter
    - `::v1`
      - `get_compact_block` now takes a `PoolTypeFilter` parameter
    - `::reader`:
      - `get_compact_block` now takes a `PoolTypeFilter` parameter
- `chain_index::types::db::legacy`:
  - `to_compact_block()`: now returns transparent data
- `chain_index`:
  - `ChainIndex::snapshot_nonfinalized_state` now returns a `Future<Output = Result<Self::Snapshot>>`
    instead of a `Self::Snapshot`
  - `NodeBackedChainIndexSubscriber`'s `ChainIndex` implementation:
      - `Snapshot` associated type is now a `ChainIndexSnapshot`
      this effects all associated methods.
  - `non_finalized_state::BestTip` renamed and relocated to
    `chain_index::types::BlockIndex` (was briefly `non_finalized_state::BlockIdent`
    earlier in the same unreleased cycle); its inner field is now named `hash`
    (previously `blockhash`), and it gains `Eq`/`Hash` derives.
- `FetchService` and `StateService` now serve the get_raw_transaction RPC through
  `ChainIndex`.
- `FetchService` and `StateService` now serve the transparent-address RPCs through
  `ChainIndex`.
- `FetchService` and `StateService` now serve `gettxoutsetinfo` through
  `ChainIndex` instead of forwarding to the backing validator. Response fields
  `transactions`, `txouts`, `total_amount`, `height` and `bestblock` agree
  with zcashd's RPC; `bytes_serialized` and `hash_serialized` follow Zaino's
  own deterministic spec.

### Deprecated
- `GetTaddressTxids` is replaced by `GetTaddressTransactions`

### Removed
- `Ping` for GRPC service
- `utils::blockid_to_hashorheight` moved to `zaino_proto::utils`
- `non_finalized_state::NonfinalizedBlockCacheSnapshot` visibility narrowed
  from `pub` to `pub(crate)`; it is no longer part of the public API.
  External consumers should use `ChainIndexSnapshot` instead.
