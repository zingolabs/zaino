# zaino-index-compact-block

The compact-block index: an append-only store of gRPC-framed `CompactBlock`
records, the loop that builds it (`CompactBlockIndexWriter`), and the service
`zaino-grpc` serves `GetLatestBlock` / `GetBlock` / `GetBlockRange` from.

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

```rust,ignore
let index = CompactBlockIndexWriter::new(store, batch_bytes);
let durable = index.durable_tip(); // for the producer's start and chain check
let service = CompactBlockService::new(index.published().served());
tokio::spawn(index.run(blocks, fees));
```

- `NAME` = `"compact_block"`. `run(blocks, fees)` is its own loop over its
  `BlockSink` subscription and its subscription to the value-balance index's
  `FeeSink`
  ([`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md)):
  one step off each per step, the block step then its fee step. Every
  `CompactTx.fee` comes from them. The two steps are asserted to match (kind,
  height, `finalized`, fees of that block), so they end on the same `Shutdown`.
- Fallible only at boot (`CompactBlockStore::open` → `StoreError`); `new` is
  infallible. `run` is infallible: it panics on a failed commit, on a tree size
  past `u32` (#549), and when value-balance's fee sink drops
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- Per step: a final block (bulk) joins `bulk` and commits once `batch_bytes`
  is held (a replay at or below the durable tip is skipped); a non-final block
  commits what bulk holds, then is applied; `Finalized { h }` commits through
  `h`; `Reorg` drops the non-finalized state; `Shutdown` commits what bulk
  holds. Commits are synchronous: `commit(through)` returns once the blocks are
  on disk. Chain identity is the producer's check, not this index's.
- `published()` (`zaino_sync::Published<ReadView>`) = the view, both tips and
  the serving gate (its task: `published().gate(tips, depth, cancel)`).

- It derives each block's commitment-tree sizes (`chainMetadata`) as the
  previous block's plus this block's commitments, so blocks must be contiguous
  (the non-final `Apply` arm asserts it). Resume reseeds the carry from the
  manifest's sizes; each applied block keeps its sizes beside its record, for
  the commit through it.
- Applied blocks sit in a `NonFinalizedState` (the non-finalized tier: readable,
  not yet fsynced) until a commit writes them to the files. See
  [`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).
- Encoding runs inline on the loop (a byte copy, not compute) at `apply`. A
  commit reuses the non-finalized record for an applied block and encodes a
  bulk block itself, on the blocking pool with the appends, fsyncs and
  manifest; the loop waits for it.
- The committed tip hash is what the producer checks the chain against at boot
  (`ProduceError::Unlinked` / `Diverged`).
- `encode_compact_block(&Block, &BlockFees, &TreeSizes)` returns the
  framed record bytes. The block carries neither fees nor tree sizes, so the
  caller supplies its fees (asserted to be that block's) and the cumulative
  `TreeSizes`.

## Serving

```rust
let service = CompactBlockService::new(index.published().served());
```

- `published().served()` (`zaino_sync::Served<ReadView>`) = the view the loop
  republishes after every step and commit, gated on `synced`. Until the index
  reaches the tip, `block`, `resident_block`, `block_at_hash` and `range`
  answer heights at or below the durable tip (final: the producer stops on a
  contradiction and never rewrites one) and return `ServeError::Syncing` for
  anything above it. A range is judged by the top it asked for (`end`, or
  `start` when descending), so it is refused rather than cut at the durable
  tip. `latest_id` stays `Syncing`.
- A test with no loop serves the files alone with
  `Served::fixed(store.reader().pin())` (the committed-only `ReadView`).
- `block(h)` returns one framed record with every pool; `latest_id()` = the
  tip's `(height, hash)`; `tip()` = the published tip, last height inclusive (`None` = nothing held), synced or not.
- RAM-only answers, for a transport to serve inline (no page read):
  `latest_id()`, whose tip is resolved when each view is published (on the
  writer's thread), and `resident_block(h)` → `Ok(Some(record))` when the
  non-finalized tier holds `h` (`Ok(None)` = ask `block`).
- `block_at_hash(h, hash)` = `GetBlock` by hash, `h` located by the block-hash
  index. It serves the record only if that record's own `hash` field is `hash`,
  and otherwise returns `HashNotFound`. The two indexes publish independently,
  so a reorg can land between the locate and the read.
- `range(start, end, pools)` (heights, both inclusive) returns a `RangeCursor`.
  `start > end` walks it top down (the proto's "decreasing height order":
  non-finalized blocks first, then file windows downward, records reversed in
  each). The top is clamped to the tip; a bottom past the tip is `NotFound`.
  There is no length cap: pepper-sync asks for a whole shard in one call, and a
  shard (2^16 notes) spans any number of blocks. The work is bounded per window
  instead (below).
- `Pools::default()` = the shielded set (no transparent), matching an empty
  `poolTypes`; `Pools::ALL` = every pool. Pruning walks each record's framing,
  without a decode. A transaction left with no pool component (no spends,
  outputs, actions, `vin` or `vout`) is dropped, for every selection including
  `Pools::ALL` (lightwalletd's `FilterTxPool`); the block itself is always
  served. `block(h)` is never filtered.

Each request pins one `ReadView` (non-finalized tier + durable mapping) for its
whole life, so a commit landing mid-stream cannot move the non-finalized/file seam.
A record's `hash` field is read by walking its framing; the walk stops before
`vtx`, so it never touches the transactions.

A served record (`block`) is zero-copy: a refcounted `Bytes` slice of the mmap.
A range window is read the same way, then projected into one buffer.
`RangeCursor::next_chunk()` yields one file window (projected as a whole) below
the seam, and one non-finalized record above it. `next_touches_disk()` says whether the next chunk reads the files;
only that step belongs on the blocking pool (behind a range-lane permit). Each
window is at most 1 MiB and is preceded by `MADV_WILLNEED`. There is no RAM
cache for the files; the page cache is the cache.

Each non-finalized block is also held projected to `Pools::default()`, computed
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

`testing` exposes `testing::block(height) -> (Block, BlockFees, TreeSizes)`,
a sample block carrying every pool, its fees (one tx, fee 5 000) and its tree
sizes, for consumers that test against a real index
(`encode_compact_block` it, `append` the record, `commit`). It enables
`zaino-primitives/testing` for `BlockHeader::for_tests`.
