# zaino-index-compact-block

The compact-block index: an append-only sequence of gRPC-framed `CompactBlock`
records, the pure step that derives one block's record (`fold`), typed reads
over any view of it (`CompactBlockReader`, plus the reads `GetBlock` and
`GetBlockRange` answer with), and the writer that builds it from the final
stream (`CompactBlockIndexWriter`).

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
  This index owns only its record encoding, `FORMAT` and `TABLES`.
- Hash → height lives in its own index,
  [`zaino-internal-block-hash-to-height`](../zaino-internal-block-hash-to-height/usage.md).
  This index answers a hash only by reading a record's own `hash` field.
- The tree sizes after the tip are not stored apart: they are the tip record's
  `chainMetadata`.
- `Schema::new(IndexKind::CompactBlock, FORMAT, network, TABLES)` = what the
  store is opened with, and what `DiskEngine::verify` checks this index's
  directory against, for `zainod verify`.

## Folding and reading

```rust,ignore
let parent = CompactBlockReader::new(view);           // any V: SequenceRead
let mut out = store.changes(block.at());              // or the parent layer's `changes`
fold(&parent, &block, &fees, &mut out)?;              // Result<(), TreeSizeOutOfRange>
```

- `fold(parent, block, fees, out)` is the index's whole state transition (in
  `writer.rs`, beside the writer loop): it appends the block's one record to
  `out`, framed, with its `CompactTx.fee`s from `fees` (asserted to be that
  block's) and its commitment-tree sizes (`chainMetadata`) = the parent tip
  record's sizes plus what the block commits. Nothing is carried between
  calls; the parent's sizes are read through the reader.
- `parent` must hold exactly the block's parent as its tip (genesis: an empty
  view), and `out` must be opened for `block`. Anything else panics
  (`compact_block: … does not extend the parent tip` / `changes opened for
  another block`): a fold onto the wrong parent would silently mis-size every
  later record.
- `Err(TreeSizeOutOfRange)` = a tree past `u32` (#549).
- `CompactBlockReader<V>` is generic over any `V: SequenceRead`: a store's
  committed view or a `zaino_persistence::LayeredView` over one (a snapshot's:
  `snap.views().compact_block()`). `block(h)` (one framed record, every pool)
  is its public read; the windowed range read backs `RangeCursor` (one window:
  at most 256 records, cut to a byte budget, never fewer than one; either
  direction). Cloning it clones the view (pointer copies).
- At the tip the NFS folds this index after value-balance (its fees,
  `zaino-nfs::fold_block`).

## Building

```rust,ignore
use zaino_index_compact_block::{CompactBlockIndexWriter, FORMAT, TABLES};

let schema = Schema::new(IndexKind::CompactBlock, FORMAT, network, TABLES);
let store = DiskEngine::new(fs).open(&path, &schema)?;
let writer = CompactBlockIndexWriter::new(store, batch_bytes, value_balance.handle());
let handle = writer.handle();               // serves only while value-balance does
let blocks = follower.subscribe(IndexKind::CompactBlock, handle.tip(), queue_bytes);
nfs.add(IndexKind::CompactBlock, handle);
tokio::spawn(writer.run(blocks, fees));     // fees: value-balance's FeeSink subscription
```

- Generic over the persistence port: `CompactBlockIndexWriter<S: Store>` with
  `S::View: SequenceRead`; zainod picks `DiskEngine`. Its name is
  `IndexKind::CompactBlock.name()` = `"compact_block"`.
- `new` asserts one record per committed height. `handle()` = the
  `IndexHandle` the NFS reads (committed view, durable tip), `requiring`
  value-balance's (its fee source).
- `run(blocks, fees)` follows the final stream (from `FinalFollower`) through
  `zaino_sync::Committer` ([the shape every writer shares](../zaino-sync/usage.md#committer)):
  per run, one fee step off value-balance's `FeeSink` per block (held ones
  included), then on the CPU pool each block not held folded onto `staged()`
  with its fees into the delta `Run::apply` opened for it. Commits: batch full
  or 1 s idle. It ends after its stream's `Shutdown` and the fee stream's.
- Fallible only at boot (the engine's `open` → `StoreError`). `run` panics on a
  failed commit, a tree size past `u32` (#549), out-of-step fees, and when
  value-balance's fee sink drops
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- `encode_compact_block(&Block, &BlockFees, &TreeSizes)` returns the framed
  record bytes `fold` stores.

## Serving

Routes read through one snapshot per request (`snap.views().compact_block()`,
`None` = disabled), at heights `≤ snap.tip()`:

- `block(h)` = one framed record, every pool (`GetBlock` is never filtered).
- `block_at(h, hash)` = `GetBlock` by hash, `h` located by the block-hash
  index: the record only if its own `hash` field is `hash`, else
  `ServeError::HashNotFound` (`Malformed` if the record will not walk).
- `resident_block(h)` (on a `LayeredView` reader) = the record when `h` sits
  in the snapshot's layer above the committed records: RAM, no page read, a
  transport may answer inline; `None` = ask `block`.
- `RangeCursor::new(reader, start, end, tip, pools)` = `GetBlockRange` of
  heights `start` to `end`, both inclusive, never past `tip` (the snapshot's).
  `start > end` walks top down (layer records first, then file windows
  downward, records reversed in each). The top is clamped to the tip; a bottom
  past it is `ServeError::NotFound`. No length cap: work is bounded per window.
- `Pools::default()` = the shielded set (no transparent), matching an empty
  `poolTypes`; `Pools::ALL` = every pool. Pruning walks each record's framing,
  without a decode. A transaction left with no pool component is dropped
  (lightwalletd's `FilterTxPool`); the block itself is always served.

The cursor holds its reader for the whole stream, so a commit or reorg landing
mid-stream cannot move the layer/committed seam. `next_chunk()` yields one file
window below the seam (one `records` read: at most 256 records, cut to 1 MiB,
always at least one, then projected into one buffer) and one layer record above
it, projected on read. `next_touches_disk()` says whether the next chunk reads
the files; only that step belongs on the blocking pool (behind a range-lane
permit). There is no RAM cache for the files; the page cache is the cache.

## Mempool rendering

`compact_tx(index, &Transaction, fee: Option<Zatoshis>) -> CompactTx` is the
record's own per-transaction encoder, exposed so a mempool transaction renders
identically (`index` = its slot in the response; `fee` = what a validator
listed it at). The wire `fee` is a `uint32` without presence: `None` (a
coinbase, an unpriced mempool transaction) and a fee of 2^32 zatoshis or more
write 0, "not provided", rather than a saturated wrong value.

No `testing` feature: consumers testing against a real index mine their blocks with
`zaino_primitives::testing::MockChain` and fold them with `fold` + `chain.fees(hash)`.
