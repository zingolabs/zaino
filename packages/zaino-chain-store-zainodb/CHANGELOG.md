# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. ZainoDB, the LMDB-backed implementation of the `zaino-chain-store`
  ports, and the on-disk vocabulary it is built from. Moved from
  `zaino-state`'s `chain_index/finalised_state/**` and `chain_index/types/db/**`
  with the implementation unchanged. See ADR-0012.
- Implementations of `ChainStoreService`, `ChainStoreReader`, `StoredBlockRead`,
  `CompactBlockRead`, `TransactionIndex`, `SpentOutputIndex`, `TxOutSetIndex`,
  `ChainStoreIngest`, and `TransparentHistoryIndex` behind
  `transparent_address_history_experimental`.
- `ZainoDbConfig` — the ZainoDB half of a store's configuration: the LMDB
  sizing and write-cadence budgets, and the network whose activation schedule
  the store builds against. Exactly the two things `ChainStoreConfig` cannot
  carry, and nothing else — notably no path, because where a store lives is the
  neutral half's business.
- `types` — the persisted shapes, each sitting with its own encoding and the
  golden test that pins its bytes, so a change to a shape and a change to what
  it serialises to cannot land apart. These are **this backend's** shapes; what
  is re-exported for `zaino-state` today is a migration measure with an end
  date, not an interface.
- A differential port suite (`tests::finalised_state::ports`): every read is
  asked twice, once through a `zaino-chain-store` port and once through the
  inherent read it replaces, and the two answers must agree. Plus a freeze round
  trip — read a chain out of one store through `StoredBlockRead`, freeze it into
  an empty one through `ChainStoreFreezeSink`, and require identical rows. That
  pair is what found the four round-trip and watermark defects listed under
  Fixed; none of them is visible to a read-only test.
- `tests`, behind the dev-dependency-only `testing` feature: the vector chain
  and the fixtures that materialise it, so `zaino-state`'s remaining suites
  compare against the same oracle rather than a second copy of it.
  `fill_store_with_blockdata` fills a store block-by-block for a test that needs
  a database *at* a height without paying for the ingest path.

### Changed
- **`FinalisedState::spawn` takes two configs**, `ChainStoreConfig` and
  `ZainoDbConfig`, replacing the `ChainIndexConfig` that was `zaino-state`'s
  struct moved wholesale. This is the convention `zaino-mempool` and
  `zaino-chain-head` already follow: the domain crate owns the configuration and
  the runtime takes it. It also removes a contradiction the old shape allowed —
  `ephemeral: bool` sitting beside a configured path, where nothing said which
  won.
- **The build-behaviour constants are configuration.**
  `LONG_RUNNING_SYNC_THRESHOLD`, `MAX_BACKGROUND_SYNC_RETRIES` and
  `BACKGROUND_SYNC_RETRY_BACKOFF` are now `ChainStoreConfig`'s
  `background_build_threshold`, `max_consecutive_failures` and `retry_backoff`:
  the same question for any store rather than anything about LMDB. The defaults
  are the values the constants held, so adopting them moves nothing.
- **Block ranges are chunked.** One cursor walk per range with
  `BLOCKS_PER_READ_TRANSACTION = 1024`, yielding a `Vec` per read transaction
  instead of one item per block — roughly a 1000× reduction in channel sends on
  `GetBlockRange`. The `StoredBlock` path gains a range walk it never had: it
  was N sequential `begin_ro_txn`s and carried a standing
  `TODO: Add separate range fetch method!`.
- **The capability bit that conflated three things is split** into
  `SPENT_OUTPUT_INDEX`, `TXOUT_SET_INDEX` and `TRANSPARENT_HIST_INDEX`.
  `get_outpoint_spenders` and the txout-set accumulator compiled
  unconditionally while routing through a bit named after a feature production
  does not enable, and `get_previous_output` routed through a fourth bit its
  only caller never paired it with. This changes behaviour during a partial
  migration and has its own migration test.
- **The reader's surface is narrowed to what has an external caller.** Twelve
  methods were reachable from outside the directory; the rest were internal
  assembly helpers that were public because the boundary was a directory. They
  are now private.
- `DbVersion::capability()` gains the missing `(1, 3)` arm. `DB_VERSION_V1` is
  `1.3.0` and fell through to `Capability::empty()`; latent only because the
  router consulted `FinalisedSource::capability()` instead.
- The ephemeral backend moves verbatim with **both** roles intact: the
  passthrough for deployments with no database, and the read shim that answers
  while a long build or migration is in progress.
- Stale prose describing a shadow-build/promote migration is deleted rather than
  ported. `set_shadow`, `extend_shadow_caps` and `promote_shadow` no longer
  exist, and `MigrationType::Major` is byte-identical to `Minor`.

### Deprecated
- `stream::ChannelStream` still carries `tonic::Status`, and so this crate still
  depends on `tonic`. The `zaino-chain-store` ports are transport-free; this
  alias and the six serving stream types built on it belong to the serving layer
  and leave with it. Nothing new should be written against it.

### Removed
### Fixed
- **The watermark now advances as the store builds.** `build_to` — the only path
  the sync worker drives — never published one, so a store that started empty
  reported "no tip" no matter how many blocks it wrote, and every read bounded by
  the watermark refused forever. Found by reading the ports from ChainIndex;
  nothing had read through those bounds before.
- **A passthrough store is no longer bounded by its own durable rows.** Reads
  routed to the ephemeral backend are answered by the validator, so the durable
  tip is not their limit — and `watermark_provenance` now reports `Passthrough`
  for a *routed* ephemeral read as well as a configured-ephemeral store, which
  it did not, so a persistent store part-way through a long build described its
  own passthrough answers as durable.
- **A non-standard transparent output survives a read/write round trip.**
  Rebuilding its locking script yielded an empty script, which reclassifies to
  an all-zero key — so a block read out of the store and frozen back in wrote a
  *different* address key for every non-standard output, the genesis coinbase
  included. It now comes back in the `tag ‖ hash` form those rows are keyed
  from, which classifies to the key it started as.
- **Per-pool value balances survive the same round trip.** They are persisted
  fields with no place in the compact projection, so they were being written
  back as `None`. `StoredBlock` now carries them.
- **A treestate the stored width cannot hold is refused, not narrowed.** The
  port layer carried a second copy of the treestate mapping whose tree-size step
  was an `as u32` cast, on the write path. It delegates to the one mapping that
  rejects.
- `get_txid` reports an out-of-range transaction index as missing data rather
  than as a malformed row, which is what lets `txid_at` answer `None` for a
  position past the end of a block — the contract it already documented.
- The coinbase null prevout no longer reaches the wire `vin`. Found by moving
  the finalised-state suite next to the code it tests.
- A migration fixture no longer resolves a hard-coded path that stopped
  existing.

### Security
- The integrity machinery moved byte-for-byte, and was proved to have done so:
  golden hex vectors for every `ZainoVersionedSerde` implementation were checked
  in before the move and pass unchanged after it. Per-row BLAKE2b-256 checksums
  over `encoded_key ‖ encoded_value`, the version-searching `verify`, the
  background validator, the startup spent-table sweep and the migration
  completion gate are all unchanged. A build-time assertion now ties
  `DB_SCHEMA_V1_HASH` to `blake2b(DB_SCHEMA_V1_TEXT)`, closing the one gap where
  the drift detector could itself drift.
