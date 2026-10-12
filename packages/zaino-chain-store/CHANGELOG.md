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

## [0.2.0] - 2026-10-09
### Added
- `StoredTxOut::from_output` reduces a domain transparent output to what the store indexes, so every consumer classifies a script the same way.
### Changed
- `ChainStoreService` and `ChainStoreReader` report health through `zaino_component::StatusSource` rather than a `status` method of their own, so a store is a component a supervisor can observe alongside every other subsystem without knowing it is a store. Two axes rather than one: a lifecycle phase the runtime moves, and a health condition that never overwrites it.
  _Migration:_ Remove `fn status(&self) -> StatusType` from both impls and add a single `impl StatusSource`, returning a `ComponentStatus` built from a `ComponentName`, a `Lifecycle` and a `Health`. Where a type is its own reader, one impl serves both ports. Callers reading a status through a bare `.status()` may need `StatusSource::status(&x)` if an inherent method of the same name still shadows the trait.
- `StoredAddress` is gone: transparent outputs are keyed by `zaino_primitives::types::TransparentAddressKey`, the same key the chain head reports its half of an address's history under, so a consumer merging the two joins them without translating.
  _Migration:_ Replace `StoredAddress` with `zaino_primitives::types::TransparentAddressKey`, which has the same `hash` and `script_type` fields and the same `new` and `is_standard` methods, plus `from_script`. `StoredTxOut::address` and `TransparentHistoryQuery::addresses` now hold the new type; neither struct is `#[non_exhaustive]`, so literal construction sites need the rename too.
- `ChainStoreFreezeSink::freeze` now takes `FrozenBlock`, a stored block without its chainwork. Cumulative work needs an unbroken chain below a block, so the store derives its own rather than trusting a value measured elsewhere — a field a caller could fill is a field a caller could fill wrongly.
  _Migration:_ Drop the `chainwork` field when building the batch: a `StoredBlock` becomes a `FrozenBlock` by carrying `header`, `transactions` and `tree_roots` across. Any value a caller was computing for it can go — the store derives its own, and a caller's was measured from somewhere else.
- `ChainStoreError::FreezeGap` — a frozen batch starting above `tip + 1` is refused rather than written into a hole, and carries both the store's tip and the first height it could not take so a caller can repair it. Previously the store stopped silently and returned `Ok(())`, which left it never advancing again while every subsequent freeze also succeeded.
  _Migration:_ `ChainStoreError` is not `#[non_exhaustive]`, so a downstream `match` over it must add a `FreezeGap` arm. Handle it by building to `first_frozen - 1` through `ChainStoreIngest::build_to` and freezing the batch again; it is a routine handover state, not a failure, so classifying it as an internal error is only right on a path that cannot repair it.
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`

## [0.1.1] - 2026-09-26
### Changed
- dependency `zaino-primitives` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
### Internal
- Address balances use the checked zatoshi quantity types.

## [0.1.0] - 2026-09-11

### Added
- New crate. The domain half of the finalised-state subsystem: vocabulary and
  ports for everything below the reorg seam, with no runtime and no storage. The
  LMDB implementation is `zaino-chain-store-zainodb`. See ADR-0012.
- `ChainStoreReader` — the universal surface, and the only one every store has:
  `watermark`, `capabilities`, `schema`, `block_hash`, `block_height`, `status`.
  Both hash↔height directions are load-bearing: resolving identity from the
  index while fetching bytes from the validator is what makes a block range
  reorg-safe.
- `ChainStoreService` — the handle. `reader()`, `status()` and
  `subscribe_watermark()`, and nothing else. Reads live on the reader.
- `StoredBlockRead` and `CompactBlockRead` — the block reads, chunk-first:
  `blocks_chunk` / `compact_chunk` return one read transaction's worth,
  `blocks_stream` / `compact_stream` return an opaque `impl Stream` of chunks —
  no allocation and no virtual dispatch to hand one across the port, at the cost
  of an implementation returning exactly one stream type per method and a
  consumer pinning it (`std::pin::pin!`, on the stack). `use<Self>` excludes the
  `&self` lifetime, so the stream is `'static` and can be moved into a spawned
  task. There is
  deliberately **no** single-block method; a single block is a chunk of one.
  Two traits rather than one because `PoolFilter` is pushed *into* the compact
  read — it decides which cursors open — so deriving compact blocks from stored
  ones would make a sapling-only wallet decode orchard and ironwood per block.
- `TransactionIndex` — `tx_position` and `txid_at`, both returning `Option`: a
  miss is a domain answer, not an error.
- `SpentOutputIndex` — `outpoint_spenders`, `previous_outputs` and
  `transparent_outputs` are batched, because the call sites they replace looped
  a singular form with one await per input. `outpoint_spenders` returns the
  spender's txid alongside its position, halving seam traffic on the hot path.
  `unspent_output` is first-class rather than two calls across two capability
  routes, one of which errored on absence.
- `TxOutSetIndex::txout_set` — a *partial fold* the consumer completes with the
  head's blocks, not an RPC answer.
- `ChainStoreIngest` — `build_to`, `rewind_to`, `wait_until_built`, `shutdown`.
  `build_to` takes a target height and **no source**: the store owns its source,
  so a consumer cannot repoint a running store.
- `ChainStoreFreezeSink::freeze` — takes a slice, so the port does not encode
  which write path an implementation dispatches to. The counterpart to the chain
  head's `ChainHeadFreezeEvents`; that stream is best-effort, so an
  implementation must be idempotent on `(height, hash)`.
- `ChainStoreSource` — the driven port, a bound alias over `GetBestBlockHeight`,
  `GetRawBlock` and `GetCommitmentTreeRoots` with a blanket impl. Not
  `GetTransaction`: nothing in the finalised state calls it. A compile-time
  bound test asserts `ZebraValidator` satisfies it.
- `StoredTx` — a compact transaction plus the per-pool value balances an index
  persists beside it. `StoredBlock.transactions` carries these rather than bare
  `PreIndexCompactTx`, because the compact protocol has no value balance and a
  store does: a block read through `StoredBlockRead` and written back through
  `ChainStoreFreezeSink` has to describe the same block, and without the
  balances it did not.
- `StoredBlock`, `StoredTxOut`, `StoredAddress`, `SpenderRef`, `PoolFilter`,
  `StoreWatermark`, `StoreCapabilities`, `StoreSchema`,
  `ChainStoreConfig`, `ChainStoreError`, `ChainStoreSourceError`.
  `ChainStoreConfig` is the backend-neutral half of a store's configuration and
  is what every implementation takes; an implementation pairs it with its own
  type for what a domain crate cannot name (`ZainoDbConfig` is ZainoDB's). Its
  fields are private and three of the four numeric knobs are `NonZero`, matching
  `MempoolConfig` and `ChainHeadConfig` — and where a store lives and whether it
  holds anything are one `Option<PathBuf>`, so a store configured both to hold
  nothing and to hold it somewhere is unrepresentable rather than resolved by
  whichever check runs first.
  `StoredTxOut` and `StoredAddress` are deliberately not `TransparentOutput` and
  `TransparentAddress`: on disk there is a 20-byte key and a type tag, the
  script is unrecoverable, and a type that cannot express `NonStandard` would
  hide that.
- `txout_set` — the UTXO-set commitment: a canonical entry encoding and a
  multiset hash over it. In the domain crate rather than an implementation
  because every store must produce the same digest for the same set, and
  whatever merges finalised and recent answers must extend that same digest.
- `TransparentHistoryIndex` and the `transparent` module, behind
  `transparent_address_history_experimental`. `address_effects` is one merged
  call mirroring the chain head's, and is always range-bounded so the two
  contributions cannot double-count across the seam. There is deliberately no
  balance method: a net signed delta loses what a consumer needs to reconcile a
  cross-seam spend.

### Changed
- `ChainStoreError::AboveWatermark` distinguishes "not mine to answer" from
  "absent". A read past the watermark is not a miss — the block very likely
  exists, in the chain head.
- The watermark bounds a read only when `provenance` is `Durable`. A
  `Passthrough` store answers from the validator rather than from what it holds,
  so bounding it by its own durable rows would refuse questions it can answer —
  for the whole of a long initial sync, which is exactly when a node depends on
  passthrough to stay useful.
- Ranges are declared **ascending only**, and a height hole inside one is an
  error rather than a silent skip. The chain head's equivalent skips holes;
  for a client's block range, silently truncating a sync is the worse failure.
- No error type here is `tonic::Status`. Mapping to a transport status is
  `zaino-serve`'s job.
- The finalised store's read path is instrumented. A corrupt row is logged at
  `warn` with its typed cause and counted as `zaino.db.corrupt_rows_total`; the
  two chunked reads carry a span and a latency histogram
  (`zaino.db.compact_read_seconds`, `zaino.db.block_read_seconds`). Previously a
  read failure fell through to the validator silently, so a damaged store was
  indistinguishable from one merely behind. Histograms are behind the
  `prometheus` feature, as on the write path; the log is not.
- `PoolFilter` holds the shielded pools as a set and transparent as one flag,
  rather than four parallel bools. `all`, `none`, `Default`, `with_pool` and
  `includes` no longer enumerate the pools, so adding one is a single entry in
  `ShieldedPool::ALL` and no change here. The public API is unchanged; the type
  is now `Copy`.
- `StoreAddressEffects::net_value` returns `Option<SignedZatoshis>` rather than
  a bare `i64`. It is the domain's signed-delta quantity, and the sum is now
  checked at every addition instead of an unchecked `sum::<i64>()` over
  `u64 as i64` casts, so an effect set totalling past the money supply is
  refused rather than reported as a plausible figure.
- Each read port carries a `CAPABILITY` associated const naming the
  `StoreCapability` it answers for, so a store assembles its advertised set by
  reading it off the port rather than by choosing a variant by hand. The two
  could previously drift in both directions — advertising an index the store
  does not serve, or serving one it never advertises — and both compiled.
- `StoreCapabilities` is a bit set rather than a sorted `Vec`, and `new` takes
  any `IntoIterator<Item = StoreCapability>`. The set is closed and small, so
  membership is a mask test and the type is now `Copy` with no allocation.
  `StoreCapability::ALL` enumerates the closed set.
- `ChainStoreError::CorruptRow` names a row that is present and readable but
  holds a value the domain cannot express — a height above the protocol
  maximum, an amount above the money supply, a tag naming no script type. These
  previously reported as `MissingRow`, which means an index points at a row that
  is not there. The two want different repairs: a dangling index entry is
  rebuilt from the rows it references, a corrupt value has to be refetched and
  rewritten. Construct it with `ChainStoreError::corrupt_row` /
  `corrupt_row_because`.
- `ChainStoreError` and `ChainStoreSourceError` are no longer `Clone`,
  `PartialEq` or `Eq`. Those derives forced every cause to be flattened into a
  `String`, so `Error::source()` returned `None` for exactly the variants whose
  job is telling an operator what broke. `ChainStoreError::Backend` and all four
  `ChainStoreSourceError` variants now carry an optional boxed `#[source]`.
  Construct them through `ChainStoreError::backend`/`backend_because` and
  `ChainStoreSourceError::unavailable`/`not_ready`/`inconsistent_data`/`commit`/
  `commit_because` rather than by naming the variant. Compare errors by matching
  the variant, which is what an equality assertion on them was really doing.

### Deprecated
- `StoreCapabilities` / `StoreCapability` are **interim**, and their own
  documentation says so. They surface the backend's internal routing model —
  one bit per storage trait — so `ChainIndex` keeps working until the chain view
  lands with a domain-shaped serviceability manifest. Do not build on them.

### Removed
### Fixed
