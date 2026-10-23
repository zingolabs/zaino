# zaino-index-compact-block

The compact-block index. It backs `GetBlock` and `GetBlockRange` (plus the
deprecated `GetBlockRangeNullifiers`) from one record per height: the block's
gRPC-framed `CompactBlock`, ready to send.

## On disk

One `Variable` sequence, `"blocks"`, through the persistence port
([`zaino-persistence`](../zaino-persistence/usage.md)): record `h` = the block
at height `h`. zainod stores it with `DiskEngine`:

```text
MANIFEST      durable tip + each file's seal (the engine's)
blocks.dat    records back to back, height order, each [0x00][len u32 BE][CompactBlock protobuf]
blocks.idx    end offset per record (record h = end(h − 1)..end(h))
```

- The index is positional: a height is never stored as a key. `blocks.idx`
  turns a height into a byte span, and a height range is one contiguous span
  of `blocks.dat`.
- A record is the exact bytes that go on the wire (a 5-byte gRPC frame header
  plus the message), so serving moves bytes without decoding or re-encoding.
  Every pool is stored, so any `Pools` subset is servable by pruning on read.
- Page checksums, the commit protocol, crash recovery and the open-time checks
  are the engine's ([zaino-persistence](../zaino-persistence/usage.md#the-commit-and-crash-contract)).
  This index owns only its record encoding and `schema(network)`.
- The cumulative tree sizes after a block are not stored apart: they are that
  record's `chainMetadata`.
- Hash → height lives in its own index,
  [`zaino-internal-block-hash-to-height`](../zaino-internal-block-hash-to-height/usage.md).
  This index checks a hash only against a record's own `hash` field.

## What one block contributes

```rust,ignore
let parent = CompactBlockReader::new(view);  // any V: SequenceRead
let mut out = store.changes(block.block_ref());     // or the parent layer's `changes`
fold(&parent, &block, &fees, &mut out);
```

`fold` appends the block's one record, built by
`encode_compact_block(&Block, &BlockFees, &TreeSizes)`:

- `CompactTx.fee` comes from `fees`, utxo-set's `BlockFees` for this block
  (asserted). A transparent input does not carry the value it spends, so the
  fee needs the UTXO set that
  [`zaino-index-utxo-set`](../zaino-index-utxo-set/usage.md)
  keeps.
- `chainMetadata` = the parent tip record's sizes plus what the block commits.
  A domain `Block` carries no tree sizes, and asking the validator per block
  would be a round trip per block during sync.
- `parent` must hold exactly the block's parent as its tip (genesis: an empty
  view), and `out` must be opened for `block`. Anything else panics, because a
  fold onto the wrong parent would mis-size every later record.

## Building

Wired by `zaino_nfs::writer` ([zaino-nfs wiring](../zaino-nfs/usage.md#wiring-nfs)), its fees
a subscription to utxo-set's `FeeSink`.

- Generic over the persistence port: `CompactBlockIndexWriter<S: Store>` with
  `S::View: SequenceRead`. zainod picks `DiskEngine` and enables it with
  `index.compact_block` (utxo-set comes with it).
- `run` follows the finalized stream ([the writer shape](../zaino-sync/usage.md#writer-loop)).
  It pops one fee step per block, held blocks included, so the two streams stay
  in step whichever index is ahead. Blocks it does not hold are folded and
  `zaino_sync::apply`d. It commits on a `FinalizedTip` block, a full buffer and
  `Shutdown`, and ends after both streams' `Shutdown`.
- At the tip the NFS folds utxo-set first and hands its fees straight to
  this `fold` (`zaino-nfs::fold_block`).
- Fallible only at boot (`StoreError` from `open`). `run` panics on a failed
  commit, a commitment tree past 2^32 leaves, out-of-step fees, or a fee stream that ends early
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).

## Serving

Routes read through one `GrpcSnapshot` per request (`at.views().compact_block()`,
`None` = disabled), at heights `≤ at.answers_through(IndexKind::CompactBlock)`.
`CompactBlockReader<V>` works
over any `V: SequenceRead`: a store's durable view, or an `NfsPublication`'s
`OverlayView` with the blocks above its durable tip layered on top.

- `block(h)` = one framed record, every pool. `GetBlock` is never filtered.
- `block_at(h, hash)` = `GetBlock` by hash, with `h` found by the block-hash
  index. It returns the record only if its `hash` field matches, else
  `ServeError::HashNotFound` (`Malformed` if the record will not walk).
- `resident_block(h)` returns the record when `h` sits in the NFS
  in-memory layer, so a transport can answer inline. `None` = ask `block`.
- `RangeCursor::new(reader, start, end, tip, pools)` = `GetBlockRange` from
  `start` to `end`, both inclusive. `start > end` walks downward. The top is
  clamped to `tip`; a bottom past it is `ServeError::NotFound`. There is no
  length cap, because work is bounded per chunk.
- `Pools::default()` = the shielded pools (no transparent), matching an empty
  `poolTypes`. `Pools::ALL` = every pool. Pruning walks each record's protobuf
  framing without decoding it. A transaction left with no pool component is
  dropped (lightwalletd's `FilterTxPool`); the block is always served.

The cursor holds its reader for the whole stream, so a commit or reorg landing
mid-stream cannot move the seam between the in-memory layer and the files.
`next_chunk()` yields either one file window (at most 256 records, cut to
1 MiB, always at least one) or one layer record. `next_touches_disk()` tells
the transport whether the next chunk reads files, since only that step belongs
on the blocking pool. There is no RAM cache for the files; the page cache is
the cache.

### What it does not answer

- `GetTransaction`: compact records drop most of a transaction, so full
  transactions come from the validator (`zaino-grpc`).
- `GetLatestBlock` reads nothing here: it is the served tip.

## Mempool rendering

`compact_tx(index, &Transaction, fee: Option<Zatoshis>) -> CompactTx` is the
record's own per-transaction encoder, exposed so a mempool transaction renders
identically (`index` = its slot in the response, `fee` = its mempool entry's). The wire `fee` is a `uint32` with no presence bit, so `None` (a
coinbase or an unpriced mempool transaction) and any fee of 2^32 zatoshis or
more both write 0, "not provided", rather than a wrong value.

## Testing

There is no `testing` feature. Mine blocks with
`zaino_source::testing::MockChain` and fold them with `fold` and
`chain.fees(hash)`.
