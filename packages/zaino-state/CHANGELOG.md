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
- `local_cache::compact_block_with_pool_types`
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
  - `non_finalized_state::BestTip` renamed to `non_finalized_state::BlockIdent`

### Deprecated
- `GetTaddressTxids` is replaced by `GetTaddressTransactions`

### Removed
- `Ping` for GRPC service
- `utils::blockid_to_hashorheight` moved to `zaino_proto::utils`
- `non_finalized_state::NonfinalizedBlockCacheSnapshot` visibility narrowed
  from `pub` to `pub(crate)`; it is no longer part of the public API.
  External consumers should use `ChainIndexSnapshot` instead.
