# zaino-index-compact-block

The compact-block index: an append-only sequence of gRPC-framed `CompactBlock`
records, the pure step that derives one block's record (`fold`), typed reads
over any view of it (`CompactBlockReader`), the loop that builds it
(`CompactBlockIndexWriter`), and the service `zaino-grpc` serves
`GetLatestBlock` / `GetBlock` / `GetBlockRange` from.

## On disk

One `Variable` sequence, `"blocks"`, through the persistence port
([`zaino-persistence`](../zaino-persistence/usage.md)): record `h` = the block
at height `h`. zainod stores it with `DiskEngine`:

```text
MANIFEST      committed tip + each file's seal (the engine's)
blocks.dat    records, each [0x00][len u32 BE][CompactBlock protobuf]
blocks.idx    end offset per record
```

- A record is the exact bytes that go on the wire (a 5-byte gRPC frame header
  plus the message), stored with every pool so any `Pools` subset is servable.
- Page checksums, the commit protocol, crash recovery and the open-time checks
  are the engine's ([`docs/design/durability.md`](../../docs/design/durability.md)).
  This index owns only its record encoding and `schema(network)`.
- Hash → height lives in its own index,
  [`zaino-internal-block-hash-to-height`](../zaino-internal-block-hash-to-height/usage.md).
  This index answers a hash only by reading a record's own `hash` field.
- The tree sizes after the tip are not stored apart: they are the tip record's
  `chainMetadata`.
- `schema(network)` = what the store is opened with, and what
  `DiskEngine::verify` checks this index's directory against, for
  `zainod verify`.

## Folding and reading

```rust,ignore
let parent = CompactBlockReader::new(view, network); // any V: SequenceRead
let changes = fold(&parent, &block, &fees)?;          // Result<Changes, TreeSizeOutOfRange>
```

- `fold(parent, block, fees)` is the index's whole state transition, pure: the
  block's one record, framed, with its `CompactTx.fee`s from `fees` (asserted
  to be that block's) and its commitment-tree sizes (`chainMetadata`) = the
  parent tip record's sizes plus what the block commits. Nothing is carried
  between calls; the parent's sizes are read through the reader, so a fold
  after a restart or a reorg reads the record it now builds on.
- `parent` must hold exactly the block's parent as its tip (genesis: an empty
  view). Anything else panics ("does not extend the parent tip"): a fold onto
  the wrong parent would silently mis-size every later record.
- `Err(TreeSizeOutOfRange)` = a tree past `u32` (#549).
- `CompactBlockReader<V>` is generic over any `V: SequenceRead`: a store's
  committed view, `zaino_persistence::TieredView`, or a layer over either.
  `tip()` (`Option<BlockRef>`), `block(h)` (one framed record, every pool) and
  `range(first, last, budget)` (one window: at most 256 records, cut to
  `budget` bytes, never fewer than one; either direction) are its reads.
  Cloning it clones the view (pointer copies).

## Building

```rust,ignore
let store = DiskEngine::new(fs).open(&path, &zaino_index_compact_block::schema(network))?;
let index = CompactBlockIndexWriter::new(store, batch_bytes);
let durable = index.durable_tip(); // for the producer's start and chain check
let service = CompactBlockService::new(index.published().served());
tokio::spawn(index.run(blocks, fees));
```

- Generic over the persistence port: `CompactBlockIndexWriter<S: Store>` with
  `S::View: SequenceRead`; zainod picks `DiskEngine`. Its name is
  `IndexKind::CompactBlock.name()` = `"compact_block"`.
- `run(blocks, fees)` is its own loop over its `BlockSink` subscription and its
  subscription to the value-balance index's `FeeSink`
  ([`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md)):
  one step off each per step, the block step then its fee step. Every
  `CompactTx.fee` comes from them. The two steps are asserted to match (kind,
  height, `finalized`, fees of that block), so they end on the same `Shutdown`.
- Fallible only at boot (the engine's `open` → `StoreError`); `new` is
  infallible. `run` is infallible: it panics on a failed commit, on a tree size
  past `u32` (#549), and when value-balance's fee sink drops
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- Storage tiers are `zaino_persistence::Tiered`
  ([§5](../../docs/design/persistence-engine.md#5-tiering)): each block is
  `fold`ed onto a reader over everything held. A final block is staged (a
  replay at or below the durable tip is skipped) and committed once
  `batch_bytes` of source blocks are staged; a non-final block commits what is
  staged, then is applied; `Finalized { h }` commits through `h`; `Reorg` drops
  every applied block (the winner folds onto the durable tip record);
  `Shutdown` commits what is staged. Commits run on the blocking pool; the loop
  waits for them. Chain identity is the producer's check, not this index's.
- `published()` (`zaino_sync::Published<CompactBlockReader<TieredView<V>>>`) =
  the reader, both tips and the serving gate (its task:
  `published().gate(tips, depth, cancel)`).
- Folding runs inline on the loop, once per block, staged or applied: serving
  and the commit read the same bytes.
- The committed tip hash is what the producer checks the final verified chain
  against at boot (`ProduceError::Diverged`).
- `encode_compact_block(&Block, &BlockFees, &TreeSizes)` returns the
  framed record bytes `fold` stores. The block carries neither fees nor tree
  sizes, so the caller supplies its fees (asserted to be that block's) and the
  cumulative `TreeSizes`.

## Serving

```rust
let service = CompactBlockService::new(index.published().served());
```

- `published().served()` (`zaino_sync::Served<CompactBlockReader<TieredView<V>>>`)
  = the reader the loop republishes after every step and commit, gated on
  `synced`. Until the
  index reaches the tip, `block`, `resident_block`, `block_at_hash` and `range`
  answer heights at or below the durable tip (final: the producer stops on a
  contradiction and never rewrites one) and return `ServeError::Syncing` for
  anything above it. A range is judged by the top it asked for (`end`, or
  `start` when descending), so it is refused rather than cut at the durable
  tip. `latest_id` stays `Syncing`.
- A test with no loop serves committed records with
  `Served::fixed(testing::committed(store, n))` (feature `testing`).
- `block(h)` returns one framed record with every pool; `latest_id()` = the
  tip's `(height, hash)`; `tip()` = the published tip, last height inclusive
  (`None` = nothing held), synced or not.
- RAM-only answers, for a transport to serve inline (no page read):
  `latest_id()`, the view's tip block, and `resident_block(h)` →
  `Ok(Some(record))` when `h` is held above the durable tip (`Ok(None)` = ask
  `block`).
- `block_at_hash(h, hash)` = `GetBlock` by hash, `h` located by the block-hash
  index. It serves the record only if that record's own `hash` field is `hash`,
  and otherwise returns `HashNotFound`. The two indexes publish independently,
  so a reorg can land between the locate and the read.
- `range(start, end, pools)` (heights, both inclusive) returns a
  `RangeCursor<V>`. `start > end` walks it top down (the proto's "decreasing
  height order": held blocks first, then file windows downward, records
  reversed in each). The top is clamped to the tip; a bottom past the tip is
  `NotFound`. There is no length cap: pepper-sync asks for a whole shard in one
  call, and a shard (2^16 notes) spans any number of blocks. The work is
  bounded per window instead (below).
- `Pools::default()` = the shielded set (no transparent), matching an empty
  `poolTypes`; `Pools::ALL` = every pool. Pruning walks each record's framing,
  without a decode. A transaction left with no pool component (no spends,
  outputs, actions, `vin` or `vout`) is dropped, for every selection including
  `Pools::ALL` (lightwalletd's `FilterTxPool`); the block itself is always
  served. `block(h)` is never filtered.

Each request pins one `CompactBlockReader` (held blocks + durable mapping) for
its whole life, so a commit landing mid-stream cannot move the held/committed
seam. A
record's `hash` field is read by walking its framing; the walk stops before
`vtx`, so it never touches the transactions.

A served record (`block`) is zero-copy: a refcounted `Bytes` slice of the mmap
or of the held record. A range window is read the same way (one `records`
read: at most 256 records, one `MADV_WILLNEED` over them), cut to 1 MiB of
records (always at least one), then projected into one buffer.
`RangeCursor::next_chunk()` yields one file window below the seam, and one held
record above it, projected on read. `next_touches_disk()` says whether the next
chunk reads the files; only that step belongs on the blocking pool (behind a
range-lane permit). There is no RAM cache for the files; the page cache is the
cache.

## Mempool rendering

`compact_tx(index, &Transaction, fee: Option<Zatoshis>) -> CompactTx` is the
record's own per-transaction encoder, exposed so a mempool transaction renders
identically (`index` = its slot in the response; `fee` = what a validator
listed it at). The wire `fee` is a `uint32` without presence: `None` (a
coinbase, an unpriced mempool transaction) and a fee of 2^32 zatoshis or more
write 0, "not provided", rather than a saturated wrong value.

## Features

`testing` exposes `testing::block(height) -> (Block, BlockFees)`, a sample
block carrying every pool and its fees (one tx, fee 5 000), and
`testing::committed(store, n) -> CompactBlockReader<TieredView<V>>`:
`block(0..n)` folded and committed to `store` in one commit, served as the
index serves them (tree sizes after `h` = `(h + 1) × (1, 1, 2)`).
Blocks come from one deterministic `zaino_primitives::testing::Chain` (it
enables `zaino-primitives/testing`), every block carrying the same sample
transaction, so `block(h)` links onto `block(h - 1)` and its hash is real
(never `[h; 32]`).
