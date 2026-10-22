# zaino-internal-block-hash-to-height

The block hash → height index. Every `BlockID.hash` request resolves through
it: `GetBlock` and `GetTreeState` by hash, in `zaino-grpc`. It answers no gRPC
method of its own. It names a height, and the index serving the answer confirms
it holds the same block there.

## What it stores

One map on the [persistence port](../zaino-persistence/usage.md):

```text
by_hash   hash 32 (protocol byte order) → height u32 BE      point lookups only
```

- Each block contributes one row, from its own header. `fold` reads nothing but
  the parent's tip.
- Insert only: a finalized block's height never changes. Blocks above the
  durable tip are `zaino-nfs` layers.
- A read is one point probe, and the store's per-segment filter turns most
  misses into no read at all.
- There is no height → hash direction. Height is every other index's native
  key, and the hash at a height comes from the answering index's own records.

## Wiring

Wired by `zaino_nfs::writer` ([zaino-nfs wiring](../zaino-nfs/usage.md#wiring-nfs)).

- Generic over the persistence port: `BlockHashIndexWriter<S: Store>` with
  `S::View: MapRead`. zainod picks `DiskEngine`.
- `run` follows the finalized stream ([the writer shape](../zaino-sync/usage.md#writer-loop)):
  each block not yet held is folded into a delta opened for it, then
  `zaino_sync::apply`d. `fold` panics, naming the index, on a block that does
  not extend the parent's tip or a delta opened for another block.
- Fallible only at boot (`StoreError` from `open`). `run` returns at `Shutdown`
  and panics on a failed commit
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- zainod builds it only when `index.block_hash.enabled`. Disabled, a by-hash
  `GetBlock` / `GetTreeState` is `UNIMPLEMENTED`; the by-height forms are
  unaffected.
- `zainod verify` checks the directory against
  `schema(network)`.

## Serving

- A route reads `at.views().block_hash()?.height_of(&hash)`.
  `BlockHashReader<V>` works over any `V: MapRead`: a store's durable view or
  an `NfsPublication`'s `OverlayView`.
- A hash it does not hold, or one located above the answering index's
  `IndexesAtBlock::answers_through`, is `NOT_FOUND`.
- The caller confirms the block: compact-block's `block_at` checks the record's
  `hash` field, and the tree-state route checks `Treestate.block_hash`.
