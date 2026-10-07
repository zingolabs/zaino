# zaino-internal-block-hash-to-height

The block hash ↔ height index. Every `BlockID.hash` request resolves through it:
`GetBlock` and `GetTreeState` by hash, in `zaino-grpc`. It claims no gRPC method
of its own. It names a height, and the index serving the answer confirms that it
holds the same block there.

## Wiring

```rust
use zaino_internal_block_hash_to_height::BlockHashIndexWriter;
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine};

let schema = zaino_internal_block_hash_to_height::schema(network);
let writer = BlockHashIndexWriter::new(DiskEngine::new(fs).open(&path, &schema)?, batch_bytes);
let blocks = nfs.subscribe(IndexKind::BlockHash, writer.committed(), queue_bytes);
tokio::spawn(writer.run(blocks));
```

- Generic over the persistence port: `BlockHashIndexWriter<S: Store>` with
  `S::View: MapRead`; zainod picks `DiskEngine`.
- `run` follows the final stream (`"block_hash"`) through `zaino_sync::Committer`
  ([the writer shape](../zaino-sync/usage.md#committer)): each unfolded step not
  held = `fold(&block, network)`, each folded step applied as sent.
  `committed()` = the committed-view watch the NFS reads.

## Folding and reading

- `fold(block, network) -> Changes` is the index's whole state transition,
  pure: one `by_hash` row from the header. It reads no parent state.
- `BlockHashReader<V>` is generic over any `V: MapRead` (a store's committed
  view or a `LayeredView` over one): `height_of(&BlockHash)`.
- Fallible only at boot (the engine's `open` → `StoreError`). `run` returns at
  `Shutdown` and panics on a failed commit
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- zainod builds it only when `index.block_hash.enabled`. When it is off, a
  by-hash `GetBlock` / `GetTreeState` is `UNIMPLEMENTED`; the by-height forms
  are unaffected.

## Serving

- A route reads `snap.views().block_hash()?.height_of(&hash)`; a hash it does
  not hold, or one located above the snapshot tip, is `NOT_FOUND`.
- The answer is a height on this index's chain. The caller confirms that its own
  index holds `hash` at that height: compact-block's `block_at` checks the
  record's `hash` field, and the tree-state route checks `Treestate.block_hash`.
- There is no height → hash lookup. Heights are every index's native key, and
  the hash at a height comes from the answering index's own records.

## Storage

One map on the [persistence port](../zaino-persistence/usage.md):

```text
by_hash   hash 32 (protocol byte order) → height u32 BE      point lookups only
```

- Segments, merges, the manifest and crash recovery are the engine's.
- `schema(network)` is the store's `Schema`; `zainod verify` checks the
  directory against it (`PersistenceEngine::verify`).
