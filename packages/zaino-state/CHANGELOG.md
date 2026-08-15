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

## [0.7.0] - 2026-08-14

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.6.0] - 2026-08-04

### Added
### Changed
- The six public stream types (`RawTransactionStream`,
  `CompactTransactionStream`, `CompactBlockStream`, `UtxoReplyStream`,
  `SubtreeRootReplyStream`, `AddressStream`) are collapsed to
  `pub type X = ChannelStream<T>` aliases. Their names, constructors, and
  `Stream` impls are preserved, so this is **not** a breaking change for
  typical use.
- Adopted the DRY'd `zaino-proto` proto utilities.
- Internal refactor of the error and indexer plumbing.
### Deprecated
### Removed
- **Breaking** — the public constructors `BlockData::new` and
  `BlockMetadata::new` (construct these types via struct literals instead).
- **Breaking** — `impl ZainoVersionedSerde for IndexedBlock` and
  `impl ZainoVersionedSerde for CompactTxData` (the never-adopted
  wholesale-block serde path).
### Fixed

## [0.5.0] - 2026-07-13

### Added
- The chain index tracks Ironwood (NU6.3) note-commitment treestate roots,
  storing `None` while the pool has no treestate rather than fabricating a
  root.
### Changed
### Deprecated
### Removed
### Fixed

## [0.4.0] - 2026-07-02

### Added
- `ChainIndex` / `NodeBackedChainIndexSubscriber` gain `get_outpoint_spenders` —
  for each transparent `Outpoint`, returns the txid that spent it on the best
  chain (index-aligned with the input, `None` if unspent or unknown).
- `chain_index::types::ChainScope` — new enum (`Finalised`, `FullChain`)
  selecting how far `get_outpoint_spenders` searches.
- Optional ("ephemeral") finalised state: with `ChainIndexConfig::ephemeral`,
  no finalised database is opened. Finalised reads are served by an ephemeral
  passthrough (`finalised_source::ephemeral::EphemeralFinalisedState`) directly
  from the backing `BlockchainSource`; `sync_to_height` is a no-op and
  `db_height` reports `0`.
- Background finalised-state sync and migration: `FinalisedState::sync_to_height`
  runs inline for small ranges but spawns for large ones, and version migrations
  run in the background, in both cases serving reads from an ephemeral passthrough
  meanwhile. Failed background work retries and escalates to `CriticalError`.
- `FinalisedState::wait_until_synced` — waits for in-progress background
  sync/migration to reach its target (distinct from `wait_until_ready`, which
  reflects serving-readiness).
### Changed
- `BlockchainSource` documents how it dissolves, not just that it will. Before a
  subsystem migrates, its needs sit on the trait as wire-typed *methods*; after,
  as `zaino-source` *supertraits* — so each migration converts method-surface
  into port-surface and the trait tends toward a bound with no methods of its
  own, at which point the ChainHead cutover deletes it mechanically. Written
  down because the opposite reading is available and was reached in review: a
  growing supertrait list looks like accretion, and an adapter bounded on the
  trait while calling none of its methods looks like coupling, when both are the
  *finished* state for a migrated subsystem. Also records the two things to
  notice at the end — a migrated subsystem's ports are named both here and in
  `ChainIndexSourcePorts` with nothing enforcing that they agree, and the end
  state converges on the two being duplicates.
- **The mempool is now the `zaino-mempool` / `zaino-mempool-service` subsystem.**
  `chain_index::mempool` (`Mempool`, `MempoolKey`, `MempoolValue`) and the
  `Broadcast`/`BroadcastSubscriber` map it was built on are deleted, along with
  their re-exports from the crate root. The ChainIndex now owns a tip-agnostic
  `MempoolService` and a tip-aware `CoherenceService` over it.

  The `chain_index::mempool` path is reused, deliberately: it is now
  ChainIndex's side of the mempool boundary — the private module holding the
  two adapters that wire the subsystem in — rather than a mempool
  implementation. It is named for the boundary, as `chain_head.rs` is, so the
  name survives the module's contents changing.

  The behavioural change this buys: the live reads (`getrawmempool`,
  `getmempoolinfo`, `GetMempoolTx`) no longer stall across a tip transition,
  because they are served from the tip-agnostic set; and the reads that place a
  transaction relative to a tip (`get_raw_transaction`, `get_transaction_status`,
  `GetMempoolStream`) now refuse to answer against a snapshot the mempool has
  moved past, instead of answering with a consensus branch id derived from the
  wrong height.
- `ChainIndex::get_mempool_transactions` takes raw txid suffixes
  (`Vec<Vec<u8>>`, client byte order, exactly as they arrive on the wire) and
  returns `Arc<MempoolEntry>` rather than taking hex strings and returning
  `Vec<Vec<u8>>`. Callers reach the shared buffer without a copy, and an
  over-long list or an unusably short suffix is now rejected as
  `InvalidArgument` rather than silently clamped.
- `ChainIndex::get_mempool_stream` yields `bytes::Bytes`, and is driven by the
  coherence layer's own "stream until the tip moves" loop rather than a local
  mpsc relay. `ChainIndex::get_mempool_height` / `mempool_branch_id` are gone;
  the branch id is derived from the caller's own snapshot.
- `ChainIndexErrorKind` gains `Unavailable` (retryable — the request is fine,
  Zaino's view has moved) and `InvalidArgument` (the request is wrong), mapping
  to `tonic::Status::unavailable` / `invalid_argument`. `child_process_status_error`
  is removed with the relay that used it.
- `BlockchainSource` requires the four `zaino-source` mempool ports as
  supertraits and no longer declares `get_mempool_txids`. The mempool subsystem
  reads those ports directly, so restating them here as wire-typed methods would
  convert domain types out and back for no reader.
- `NonfinalizedBlockCacheSnapshot` carries a `generation`, bumped when the best
  tip changes rather than on every publication, and exposes an `epoch()`. This
  is what the coherence layer freezes and thaws against; bumping per publication
  would churn it every sync iteration and defeat the agreement check.
- `CommonBackendConfig` / `ChainIndexConfig` carry a `mempool: MempoolConfig`,
  shared by clone so the two services see one `max_cost_bytes` cell.
- The `RawTransaction.data` served over gRPC is now `bytes::Bytes` rather than
  `Vec<u8>` at every construction site, following the proto change. The wire
  format is unchanged.
- `GetTransaction` reports height `0` — the wire sentinel for unmined — for a
  mempool transaction, rather than reporting the chain tip (which claimed it was
  mined at a height it is not in) or failing with `UnavailableNotSyncedEnough`.
- New metric `zaino.mempool.coherence_frozen_seconds`, and a sync-loop warning
  when coherence stays frozen past 120s. A freeze is the normal shape of a tip
  transition; a sustained one means tip-coherent reads have been failing with
  nothing in the log to say so.
- `chain_index::finalised_state` renames (internal, `pub(crate)`):
  - facade type `ZainoDB` -> `FinalisedState`
  - module `db` -> `finalised_source`; enum `DbBackend` -> `FinalisedSource`
    (variant `Stateless` -> `Ephemeral`), reflecting that the backing source is
    not necessarily a database
  - stateless impl `StatelessFinalisedState` -> `EphemeralFinalisedState`
    (`db/stateless.rs` -> `finalised_source/ephemeral.rs`)
- `chain_index::non_finalised_state` now caps in-memory retention at
  `MAX_NFS_DEPTH` blocks below the tip, so the cache cannot grow unbounded when
  the finalised `db_height` lags (background sync) or is pinned at `0`
  (ephemeral mode).
- The finalised-state bulk-sync write-batch flush interval is now configurable
  via `storage.database.sync_checkpoint_interval` (was a fixed 60s; default now
  120s). Under `NO_SYNC` this also bounds the window of unflushed writes at risk
  on a hard kill / eviction; lower it to shrink that window.
- The txout-set accumulator rebuild now sizes its in-memory spent set from a
  dedicated `storage.database.accumulator_rebuild_memory_size` budget instead of
  reusing `sync_write_batch_size`, so the bulk-sync block buffer and the rebuild
  can no longer inflate each other's peak memory.
- **Breaking** — all 25 non-proto `ZcashIndexer` return types are now
  `zaino-primitives` domain types (ADR-0009), including those that previously
  returned `zebra_rpc::methods::*`. Two exceptions remain by decision:
  `z_getblock` and `getrawtransaction` still return zebra's presentation
  shapes, which are built from block bytes plus chain facts using zebra's own
  builders. The zcashd-shaped JSON now lives in `zaino-serve`.
- **Breaking** — `NodeBackedIndexerService` is constructed over the
  `zaino-source-zebra` stack. `ValidatorConnector` and its
  `spawn_fetch`/`spawn_state` are gone; `ValidatorSource<V>` /
  `ZebraValidatorSource` replace them.
- `BlockchainSource` (`chain_index::source`) is now documented as **temporary
  scaffolding** — ChainIndex's driven port, kept so the crate keeps working
  while the `zaino-source` ports are wired in underneath, with a "do not
  extend" note. Its signatures now carry domain types. It shrinks as each
  ChainIndex subsystem moves onto the real ports and is deleted with the last
  of them.
- `ValidatorSource<V>` is generic over the ports, so the production composite
  and the test mocks (`MockchainSource`, `ProptestMockchain`) reach ChainIndex
  through the same conversion code. `mockchain_tests.rs` and
  `proptest_blockgen.rs` therefore now exercise the production conversion
  layer rather than a parallel implementation of it.
- `chain_index::source_ports::ChainIndexSourcePorts` and
  `source_caps` — per-consumer capability aliases, declared here rather than in
  `zaino-source`, because an alias states a requirement of its consumer
  (ADR-0008).
### Deprecated
### Removed
- **Breaking** — `zaino_state::{Status, StatusType, NamedAtomicStatus}` and the
  `status` module behind them. The status vocabulary lives in `zaino-status`;
  a consumer that only needs to ask whether a component is ready no longer
  depends on the indexer to find out. `AtomicStatus` is deleted outright, having
  had no callers.
- `zaino-fetch` is no longer a dependency, and the crate is deleted from the
  workspace. Its transport is `zaino-rpc`, its inbound parsing
  `zaino-source-zebra-rpc`, its outbound serialization `zaino-serve`'s wire
  module, and its legacy protocol parser moved to
  `live-tests/zaino-testutils` as a test-only independent oracle.
- `chain_index::source::validator_connector` (~3,000 lines). Its
  `ReadStateService` query logic moved to `zaino-source-zebra-readstate`, where
  it is independently testable.
- Two dead `TryFrom` impls in `types/db/legacy.rs`
  (`TryFrom<(FullBlock, ..)> for IndexedBlock`, `TryFrom<(u64, FullTransaction)>
  for CompactTxData`). Both were unreachable — every call site used
  `BlockWithMetadata` or `::new` — and survived only because trait impls are
  invisible to the dead-code lint. No `types/db/**` shape changed.
- Address classification moved out to the new `zaino-address` crate;
  `error::ChainParseError` is unproducible and removed.
- The `zcashd_support` feature declaration, which gated nothing in this crate
  once the zcashd-shaped response types moved to `zaino-serve`.
### Fixed
- `LegacyRpcError` — carries a zcashd-compatible legacy code as a typed
  `source` through the error chain, so a domain rejection reaches the serving
  layer with the code clients key on rather than as a generic internal error.
- The mempool stream parses each transaction once, not twice. It deserialized
  the same bytes into a `zebra_chain` transaction and then again into
  `zaino-fetch`'s `FullTransaction` purely to reach the latter's `to_compact`;
  the domain conversion reaches the same compact shape directly. Removes an
  `.unwrap()` on the same path, and closes the in-code TODO that asked for
  exactly this.
- The finalised-state txout-set accumulator rebuild at chain tip no longer
  OOM-crashes on memory-constrained hosts. It auto-shards its in-memory spent set
  by creating-txid prefix and now enforces the per-shard budget *strictly*: each
  shard is loaded with a hard outpoint cap (range-seeking only that shard's
  contiguous key range rather than scanning the whole `spent` table), and any
  shard that would exceed the cap is bisected and retried — down to single-byte
  shards, at which point it fails with an actionable error rather than OOM-ing.
  The result is independent of the shard count.
- Startup `spent`-table integrity failures now report the offending entry's key,
  value length, and leading value bytes plus a wipe-and-re-index hint (previously
  a bare "corrupt spent entry" / "version tag N"), and the integrity and rebuild
  scans walk the cursor explicitly so a real LMDB error propagates instead of
  being swallowed (release) or `debug_assert!`-panicking (debug).
- Corrected documentation that claimed `NO_SYNC` "never corrupts the database": on
  storage that does not preserve write order (NFS, overlay filesystems, hard pod
  eviction) a crash can leave torn pages; the recovery is to wipe and re-index.
- The non-finalised state no longer overflows the worker stack when caching a
  side-chain block. `add_nonbest_block` walked a delivered block's ancestry via
  `source.get_block` with no depth bound; on the `state` backend `get_block`
  serves any block by hash (including finalised blocks below the non-finalised
  window), so a side chain rooted below the anchor recursed down to genesis and
  crashed the process. The walk is now capped at `MAX_NFS_DEPTH` (matching
  `handle_reorg`); a side chain that doesn't anchor within the window is skipped
  (best-effort — zaino does not guarantee knowledge of all sidechain data) rather
  than crashing or failing the sync.

## [0.3.0] - 2026-06-17

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
- Finalised-state catch-up sync now ingests via `DbWrite::write_blocks_to_height`
  (the tip->height fetch/build/write loop moved into the backend) and writes the
  random-keyed `spent` / `txid_location` indexes in **sorted batches** within a
  single transaction — a sequential B-tree sweep instead of a random fault per
  insert once the DB exceeds RAM. The v1.1.0 -> v1.2.0 migration's `spent`
  backfill does the same. Batches are bounded by
  `storage.database.sync_write_batch_bytes` (default 4 GiB), a block-count cap,
  and a time cap; each batch commits and fsyncs atomically, so sync and migration
  stay crash-safe and resume gap-free.
- The finalised txout-set accumulator is no longer maintained per block during
  bulk sync or migration. It is deferred and brought up to the tip after a
  catch-up run: the first build (or an unusually large gap) does a full
  sequential-scan rebuild (`DbV1::rebuild_tx_out_set_accumulator`), while a
  steady-state catch-up applies just the delta for the newly-written range
  (`DbV1::update_tx_out_set_accumulator_for_range`) — O(range) work that yields
  the identical accumulator. This removes an unbounded fan-out of random `spent`
  reads per block that stalled sync around sandblast height. Single-block appends
  (`write_block`) still maintain it incrementally; a
  `_tx_out_set_accumulator_built_height` watermark tracks freshness and selects
  the rebuild-vs-incremental path.
- Block validation moved off the write hot path: `write_block` now performs cheap
  in-memory parent-hash continuity and merkle-root checks and advances
  `validated_tip` directly, instead of a post-commit read-back. The full
  `validate_block_blocking` re-read runs at startup only (the integrity gate for
  untrusted on-disk data).
- `get_address_utxos` now bounds the number of addresses fanned out per request,
  preventing an unbounded multi-address query from amplifying backend load
  (#974).
### Deprecated
### Removed
### Fixed
- Finalised-state catch-up no longer rebuilds the txout-set accumulator from
  genesis on every poll. Because `write_blocks_to_height` rebuilt it
  unconditionally at the end of each run, every newly-finalised block triggered a
  full-chain scan (~45 min on mainnet once the DB exceeds RAM), so the node could
  never reach the tip and stayed stuck "Syncing". The accumulator is now advanced
  incrementally over just the written range in steady state, falling back to the
  full rebuild only for the first build or a large gap.
- Finalised-state DB v1.2.0: added a reverse transaction-id index
  (`txid_location`) so previous-output resolution is an O(log n) point lookup
  instead of a full scan of the `txids` table. This fixes a near-quadratic
  slowdown that made the v1.1.0 -> v1.2.0 migration appear to hang on large
  caches and progressively slowed clean sync. The migration is now a re-entrant
  **three-stage** backfill (build `txid_location`, then `spent`, then a bulk
  txout-set accumulator rebuild) with per-stage progress trackers and progress
  logging. Stage C never trusts an existing accumulator, so a partially-run
  original migration is recomputed correctly rather than corrupted.
- Finalised-state startup validation now scans blocks in ascending height order
  (previously block-hash order via the `heights` table), so `validated_tip`
  advances monotonically, gaps surface immediately, and startup no longer
  thrashes the page cache with random-access reads.
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
