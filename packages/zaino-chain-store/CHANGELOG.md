# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
