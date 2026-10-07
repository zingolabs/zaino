# zaino-internal-block-hash-to-height

The block hash ↔ height index. Every `BlockID.hash` request resolves through it:
`GetBlock` and `GetTreeState` by hash, in `zaino-grpc`. It claims no gRPC method
of its own. It names a height, and the index serving the answer confirms that it
holds the same block there.

## Wiring

```rust
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashService};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine};

let schema = zaino_internal_block_hash_to_height::schema(network);
let index = BlockHashIndexWriter::new(DiskEngine::new(fs).open(&path, &schema)?, batch_bytes);
let blocks = block_sink.subscribe(IndexKind::BlockHash.name(), queue);
let service = BlockHashService::new(index.published().served());
router = router.with_block_hash(service);
tokio::spawn(index.published().gate(tips, depth, cancel.child_token()));
tokio::spawn(index.run(blocks));
```

- Generic over the persistence port: `BlockHashIndexWriter<S: Store>` with
  `S::View: MapRead`, serving `BlockHashReader<LayeredView<V>>` /
  `BlockHashService<V>`; zainod picks `DiskEngine`.
- `BlockHashIndexWriter` runs its own loop over its `Subscription<Block>`
  ([the shape every index shares](../zaino-sync/usage.md#an-index-loop);
  `"block_hash"`). Each block's `Changes` = `fold(&block, network)`, held in
  `zaino_persistence::Tiered` until a commit.

## Folding and reading

- `fold(block, network) -> Changes` is the index's whole state transition,
  pure: one `by_hash` row from the header. It reads no parent state.
- `BlockHashReader<V>` is generic over any `V: MapRead` (a store's committed
  view or a `LayeredView` over one): `height_of(&BlockHash)`.
- Fallible only at boot (the engine's `open` → `StoreError`). `new` and `run`
  are infallible: `run` returns at `Shutdown` and panics on a failed commit
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- zainod builds it only when `index.block_hash.enabled`. When it is off, a
  by-hash `GetBlock` / `GetTreeState` is `UNIMPLEMENTED`; the by-height forms
  are unaffected.

## Serving

- `BlockHashService::locate(&hash) -> Result<Height, ServeError>`:
  `Syncing` until the index's serving gate opens (gRPC `UNAVAILABLE`),
  and `HashNotFound` for a hash on no tier (`NOT_FOUND`).
- The answer is a height on this index's chain. The caller confirms that its own
  index holds `hash` at that height: `CompactBlockService::block_at_hash` checks
  the record's `hash` field, and the tree-state route checks
  `Treestate.block_hash`. The indexes publish independently, and a reorg can
  land between the two reads.
- There is no height → hash lookup. Heights are every index's native key, and
  the hash at a height comes from the answering index's own records.
- The reader pinned once per request answers `height_of(&hash)`: held blocks
  first, then the committed store.
- A test with no live loop serves what an index published with
  `BlockHashService::new(Served::fixed((*served.pin_any()).clone()))`.

## Storage

One map on the [persistence port](../zaino-persistence/usage.md):

```text
by_hash   hash 32 (protocol byte order) → height u32 BE      point lookups only
```

- Segments, merges, the manifest and crash recovery are the engine's.
- `schema(network)` is the store's `Schema`; `zainod verify` checks the
  directory against it (`PersistenceEngine::verify`).
