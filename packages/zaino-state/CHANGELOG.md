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
stable tag `v0.1.2-zr4` (April 2025). The state-management layer was
rebuilt: the old `BlockCache` / `FinalisedState` / `NonFinalisedState`
abstractions were replaced by a new `chain_index` architecture
backed by a versioned on-disk format and a pluggable backend layer.

### Added

#### Architecture (new modules and traits)
- `backends` module: pluggable `BackendType`, `BackendConfig`, and the
  shared `CommonBackendConfig` payload now reused by
  `StateServiceConfig` / `FetchServiceConfig` (#311, #1061).
- `chain_index` module: the new top-level state interface — `ChainIndex`
  trait, `ChainIndexError` / `ChainIndexErrorKind`, `NodeBackedChainIndex`,
  `NodeBackedChainIndexSubscriber`, `ChainTipSubscriber`.
- `chain_index::finalised_state` DB layer: `DbCore` runtime, `DbV0` /
  `DbV1` versioned on-disk formats, `DbRead` / `DbWrite` traits,
  `Migration` / `MigrationType` framework, `InitError`. `DbLifecycle`
  is `pub(super)` and not part of the public API.
- `chain_index::non_finalized_state`: `NonFinalizedState`,
  `NonFinalizedSnapshot` trait, `BestChainLocation` /
  `NonBestChainLocation`.
- `source` module: `BlockchainSource` trait + `BlockchainSourceError`,
  providing a uniform blockchain-data interface across backends.
- `validator_connector::ValidatorConnector` + `NodeConnectionError`
  for managing connections to the upstream validator.
- Service-layer abstractions: `LightWalletService`,
  `StateServiceSubscriber`.

#### Block / transaction / commitment-tree types
- Primitives: `BlockHash`, `TransactionHash`, `Height`, `ChainWork`,
  `MedianTimePast`, `Outpoint`.
- Block-level: `BlockData`, `BlockHeaderData`, `BlockMetadata`,
  `BlockWithMetadata`, `IndexedBlock`, `IndexedBlockExt`,
  `BlockCoreExt`, `BlockShieldedExt`, `BlockTransparentExt`,
  `EquihashSolution`.
- Compact-formats interop: `CompactBlockExt`, `CompactOrchardAction`,
  `CompactSaplingSpend`, `CompactSaplingOutput`, `CompactTxData`,
  `OrchardCompactTx`, `SaplingCompactTx`, `TransparentCompactTx`,
  `OrchardTxList`, `SaplingTxList`, `TransparentTxList`, `TxidList`,
  `CompactSize`, `MAX_COMPACT_SIZE`.
- Transaction / address types: `TxLocation`, `GetTransactionLocation`,
  `TxInCompact`, `TxOutCompact`, `AddrHistRecord`, `AddrScript`,
  `ScriptType`, `TransparentHistExt`.
- Commitment trees: `CommitmentTreeData`, `CommitmentTreeRoots`,
  `CommitmentTreeSizes`, `TreeRootData`, `ShardIndex`, `ShardRoot`,
  `ShieldedPool`.

#### Serialization / encoding infrastructure
- `encoding` module: `read_u8`, `read_u16_be/le`, `read_u32_be/le`,
  `read_u64_be/le`, `read_i64_be/le`, `read_fixed_be/le`,
  `read_option`, `read_vec`, `read_vec_into`,
  `read_vectors_from_file` and their `write_*` counterparts; plus
  `FixedEncodedLen` trait and `serde_arrays` helper.
- `ZainoVersionedSerde` framework for forward-compatible on-disk
  serialization (#988).
- `NamedAtomicStatus` shared status helper.

#### Other additions
- `helpers`, `test_dependencies`, `legacy` modules.
- `GENESIS_HEIGHT` constant; `MempoolInfo`, `SyncError`, `UpdateError`,
  `InvalidData`, `sync_db_with_blockdata` sync types.
- `TestVectorBlockData`, `TestVectorClientData`, `TestVectorData` for
  consumers building integration tests against zaino.
- Re-exports: `zaino_common`, `zebra_chain`.
- `DonationAddress` type (wired through to `LightdInfo`).

#### Existing entries (kept from incremental updates)
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

### Deprecated
- `GetTaddressTxids` is replaced by `GetTaddressTransactions`

### Removed
- **Breaking** — old state-management abstractions removed in favor of
  the new `chain_index` architecture:
  - `BlockCache`, `BlockCacheSubscriber`, and the entire `local_cache`
    module.
  - `FinalisedState`, `FinalisedStateSubscriber`.
  - `NonFinalisedState`, `NonFinalisedStateSubscriber`.
  External consumers should migrate to `ChainIndex` /
  `NodeBackedChainIndexSubscriber` and the `chain_index::finalised_state`
  / `chain_index::non_finalized_state` submodules.
- `StatusType` — moved to `zaino_common::status`.
- `Ping` for GRPC service.
- `utils::blockid_to_hashorheight` moved to `zaino_proto::utils`.
- `non_finalized_state::NonfinalizedBlockCacheSnapshot` visibility narrowed
  from `pub` to `pub(crate)`; it is no longer part of the public API.
  External consumers should use `ChainIndexSnapshot` instead.

### Fixed
- `ChainIndexSnapshot::get_chainblock_by_hash` and
  `get_chainblock_by_height` now delegate to the underlying
  non-finalized snapshot instead of always returning `None` (#1089).
