# zaino-index-compact-block

The compact-block index: an append-only store of gRPC-framed `CompactBlock`
records, the `zaino_sync::IndexWriter` that builds it, and the service `zaino-grpc`
serves `GetLatestBlock` / `GetBlock` / `GetBlockRange` from.

## On disk

```text
MANIFEST      committed count, tip hash, tree sizes at the tip, each file's seal
blocks.dat    records, each [0x00][len u32 BE][CompactBlock protobuf]
offsets.idx   8 bytes per height: where its record ends in blocks.dat   (OFFSET)
```

- A record is the exact bytes that go on the wire (a 5-byte gRPC frame header
  plus the message), stored with every pool so any `Pools` subset is servable.
- Every file carries page checksums (`zaino_persistence::pages`).
- Commit: records and offsets appended → both files sealed → `MANIFEST` (the
  commit point; [`docs/design/durability.md`](../../docs/design/durability.md) §4).
- Hash → height lives in its own index,
  [`zaino-internal-block-hash-to-height`](../zaino-internal-block-hash-to-height/usage.md).
  This index answers a hash only by reading a record's own `hash` field.
- `CompactBlockStore::open(fs, path, network)` takes the directory's `LOCK` and
  opens each file at its seal: bytes past it dropped, a shorter file
  (`PageError::Lost`) or a bad tail page (`PageError::Tail`) refused, a foreign
  network or format `StoreError::Manifest`. Nothing else is read.
- `append(height, hash, framed)` stores `encode_compact_block`'s framed bytes
  as given, asserting the next height and one whole frame; `commit(sizes)`
  makes everything appended durable, committing `sizes` (tree sizes after the
  last record) with it.
- `committed_files(dir, network)` = every sealed file, for `zainod verify`.

## Building

`CompactBlockIndexWriter::new(store: CompactBlockStore, balances:
Subscription<BlockValueBalances>)` implements `IndexWriter<Input =
Block, View = ReadView>` (`NAME` = `"compact_block"`), subscribed to the
`zaino_sync::BlockSink`.

- `balances` = its subscription to the value-balance index's
  `ValueBalanceSink`, taken at `store.finalized_height()` before that sink is
  sealed ([`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md)).
  `deliver` pulls each block's balances (`balances_for`, paired by hash, so
  items a reorg left queued are skipped) and holds them until the block is
  encoded. Every `CompactTx.fee` comes from them.

- It derives each block's commitment-tree sizes (`chainMetadata`) as the
  previous block's plus this block's commitments, so blocks must be contiguous
  (`apply` asserts it). Resume reseeds the carry from the manifest's sizes.
- Applied blocks sit in a `NonFinalizedState` (the nonfinalised tier: readable,
  not yet fsynced) until `finalize` writes them to the files. See
  [`docs/design/precommit-state.md`](../../docs/design/precommit-state.md).
- Encoding runs inline on the writer task (a byte copy, not compute). `finalize`
  encodes each block in order, reusing the nonfinalised record for an applied
  block. The appends, fsyncs and manifest run under `zaino_sync::blocking`.
- `finalized_tip()` is the committed tip hash the follower links the next
  delivered block onto.
- `encode_compact_block(&Block, &BlockValueBalances, &TreeSizes)` returns the
  framed record bytes. The block carries neither fees nor tree sizes, so the
  caller supplies its balances (asserted to be that block's) and the cumulative
  `TreeSizes`.

## Serving

```rust
let service = CompactBlockService::new(follower.served())
    .with_max_range(max_block_range);
```

- `follower.served()` (`zaino_sync::Served<ReadView>`) = the view the follower
  republishes after every step and commit, gated on `synced`: every method
  answers `ServeError::Syncing` until the index reaches the tip.
- A test with no follower serves the files alone with
  `Served::fixed(store.reader().pin())` (the committed-only `ReadView`).
- `block(h)` returns one framed record with every pool; `latest_id()` = the
  tip's `(height, hash)`; `extent()` = the published `Extent`, synced or not.
- RAM-only answers, for a transport to serve inline (no page read):
  `latest_id()`, whose tip is resolved when each view is published (on the
  writer's thread), and `resident_block(h)` → `Ok(Some(record))` when the
  nonfinalised tier holds `h` (`Ok(None)` = ask `block`).
- `block_at_hash(h, hash)` = `GetBlock` by hash, `h` located by the block-hash
  index. It serves the record only if that record's own `hash` field is `hash`,
  and otherwise returns `HashNotFound`. The two indexes publish independently,
  so a reorg can land between the locate and the read.
- `range(from, to, pools)` returns a `RangeCursor`: `from <= to` is asserted
  (the caller orders; `zaino-grpc` parses client heights into `Height` at the
  router), `to` is clamped to the tip, a clamped span over `max_range` (default
  `DEFAULT_MAX_BLOCK_RANGE` = 131 072) is `RangeTooLarge`, never truncated.
- `Pools::default()` = the shielded set (no transparent), matching an empty
  `poolTypes`; `Pools::ALL` = every pool. Pruning walks each record's framing,
  without a decode; `Pools::ALL` serves the stored bytes untouched.

Each request pins one `ReadView` (nonfinalised tier + durable mapping) for its
whole life, so a commit landing mid-stream cannot move the nonfinalised/file seam.
A record's `hash` field is read by walking its framing; the walk stops before
`vtx`, so it never touches the transactions.

Reads are zero-copy: a served record or unprojected range window is a
refcounted `Bytes` slice of the mmap. `RangeCursor::next_chunk()` yields one
file window (projected as a whole) below the seam, and one nonfinalised record
above it. `next_touches_disk()` says whether the next chunk reads the files;
only that step belongs on the blocking pool (behind a range-lane permit). Each
window is at most 1 MiB and is preceded by `MADV_WILLNEED`. There is no RAM
cache for the files; the page cache is the cache.

Each nonfinalised block is also held projected to `Pools::default()`, computed
once at `apply`, since every synced wallet asks each tip block in that shape.
A tip range in the default pools is therefore zero-copy. When the record holds
nothing to prune, the projection is the record's own `Bytes`, so it costs no
memory.

## Mempool rendering

`compact_tx(index, &Transaction, fee: Option<Zatoshis>) -> CompactTx` is the
record's own per-transaction encoder, exposed so a mempool transaction renders
identically (`index` = its slot in the response; `fee` = what a validator
listed it at). The wire `fee` is a `uint32` without presence: `None` (a
coinbase, an unpriced mempool transaction) and a fee of 2^32 zatoshis or more
write 0, "not provided", rather than a saturated wrong value.

## Features

`testing` exposes `testing::block(height) -> (Block, BlockValueBalances,
TreeSizes)`, a sample block carrying every pool, its balances (one tx, fee
5 000) and its tree sizes, for consumers that test against a real index
(`encode_compact_block` it, `append` the record, `commit`). It enables
`zaino-primitives/testing` for `BlockHeader::for_tests`.
