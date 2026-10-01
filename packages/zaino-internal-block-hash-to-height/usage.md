# zaino-internal-block-hash-to-height

The block hash ↔ height index. Every `BlockID.hash` request resolves through it:
`GetBlock` and `GetTreeState` by hash, in `zaino-grpc`. It claims no gRPC method
of its own. It names a height, and the index serving the answer confirms that it
holds the same block there.

## Wiring

```rust
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashService, BlockHashStore};

let store = BlockHashStore::open(fs, &path, network)?;
let index = BlockHashIndexWriter::new(store, batch_bytes);
let blocks = block_sink.subscribe(BlockHashIndexWriter::NAME, queue);
let service = BlockHashService::new(index.published().served());
router = router.with_block_hash(service);
tokio::spawn(index.published().gate(tips, depth, cancel.child_token()));
tokio::spawn(index.run(blocks));
```

- `BlockHashIndexWriter` runs its own loop over its `Subscription<Block>`
  ([the shape every index shares](../zaino-sync/usage.md#an-index-loop);
  `NAME` = `"block_hash"`). It derives nothing: one hash per block, taken from
  the header. A non-final block goes into a hash → height map; a commit moves
  its hashes into the store's segments.
- Fallible only at boot (`BlockHashStore::open` → `StoreError`). `run` is
  infallible: it returns at `Shutdown` and panics on a failed commit
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
- `ReadView` (pinned once per request) answers `height_of_hash(&hash)`. It
  checks the non-finalized map first, then the committed segments.
- A test with no index loop serves the committed segments alone with
  `BlockHashService::new(Served::fixed(store.reader().pin()))`.

## Storage

```text
<dir>/
  MANIFEST      committed count, tip hash, segment list
  by_hash/      immutable sorted (hash, height) segments (zaino_persistence::lsm)
```

- `BlockHashStore::commit(&[(Height, hash)])` writes one finalised batch as one
  new segment, then the `MANIFEST` (the one commit point).
- Background merges keep the segment count bounded (about 35 on mainnet).
- The data structure, the lookup path and the merge compaction are explained in
  the module docs of `src/lib.rs`.
- `committed_files(dir, network)` lists every sealed file, for `zainod verify`.
