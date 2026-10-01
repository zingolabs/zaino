# zaino-index-tree-state

The commitment-tree index for all three shielded pools (Sapling, Orchard,
Ironwood). Backs `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots`
from one fold over the domain `Block`s the sync pipeline carries. The positional shape it instantiates
is described in
[`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §3.

## Wiring

```rust
use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateService, TreeStateStore};

let store = TreeStateStore::open(fs, &path, network)?;
let index = TreeStateIndexWriter::new(store, batch_bytes)?;
let blocks = block_sink.subscribe(TreeStateIndexWriter::NAME, queue);
let service = TreeStateService::new(index.published().served(), network);
tokio::spawn(index.published().gate(tips, depth, cancel.child_token()));
tokio::spawn(index.run(blocks));
```

- `TreeStateIndexWriter` runs its own loop over the `zaino_sync::BlockSink`
  subscription (`NAME` = `"tree_state"`): `run` follows it through `Shutdown`,
  publishing through a `zaino_sync::Published<ReadView>`. Final blocks (bulk)
  are held and folded at commit, one commit per `batch_bytes`; once following
  the tip, each `Finalized { height }` commits everything through `height` as
  it arrives. `durable_tip()` = the last committed block, for the producer's
  start and chain check.
- Fallible only at boot: `open` (`StoreError`) and `new`, which reseeds the
  running frontiers from disk (`IndexWriterError`). `run` is infallible: it
  returns at `Shutdown` and panics on a failed commit or an unfoldable block
  ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- `ReadView` binds the non-finalized tier and the committed snapshot into one publication:
  a request loads it once, so the seam between them cannot move under it.
  `treestate(height)`, `latest()` and `subtree_roots(..)` answer from it, with no
  `synced` gate; `TreeStateService` adds the gate. The index's `Published`
  (`index.published()`) is the only way to get one.
- Tests drive it as production does: steps sent into a `BlockSink`, `run`
  spawned over its subscription, state read back through `published()`.
- `network` is the operator-declared network, returned by `service.network()`
  for `TreeState.network` (Zebra on regtest reports `"test"`, so it is never
  read off the validator).
- zainod builds it only when `index.tree_state.enabled`; `zaino-grpc`'s
  `with_tree_state` claims the three methods.

## Serving

| Method | Returns |
|---|---|
| `treestate(height)` | `Treestate` with all three pools |
| `latest()` | `treestate` at the highest applied height (non-finalized included) |
| `subtree_roots(pool, start_index, max_entries)` | `Vec<SubtreeRoot>` |

- While `synced` reads `false`, `treestate(height)` still answers any committed
  height (final: the answer never changes); everything else, and any height
  above the committed tip, is `ServeError::Syncing` (gRPC `UNAVAILABLE`): one
  answer, no height or progress. `latest()` on an index holding no blocks is
  `Empty` (also `UNAVAILABLE`). A height with no record is `NotFound`; stored
  nodes that will not rebuild are `Inconsistent` (a fold bug).
- Heights arrive as `Height` (range-checked by the caller).
- Every pool is emitted at every height as
  `zcash_primitives::merkle_tree::write_commitment_tree` of the real tree
  (`000000` when empty), never an empty field: clients map an absent field onto
  an empty tree silently. `final_root` stays unset.
- `subtree_roots`: `max_entries == 0` = to the end; `start_index == count` =
  `Ok(vec![])`. Each root carries its completing block (`BlockRef`: height from
  the subtree entry, hash from that height's record).
- No hashing on the read path: each request is one 48 B height record plus
  ≤ 33 node reads per pool.
- `GetTreeState` by `BlockID.hash` resolves through the block-hash index
  (`zaino-internal-block-hash-to-height`) in `zaino-grpc`; this index answers by
  height and the router confirms the hash it holds there.
- `pin()` → the latest synced publication (`Arc<ReadView>`, one per
  publication), with the same `treestate`, `latest` and `subtree_roots` on it.
  `is_non_finalized(h)` names the ~1000 heights every synced wallet asks about.
  `zaino-grpc` keys its per-publication memos on the `Arc`: tip tree states and
  whole root lists are framed once per block, not once per wallet.

## Storage

```text
<dir>/
  MANIFEST                   count, tip hash, every file's seal
  heights.idx                48 B/height: hash, time, three cumulative tree sizes
  sapling/ orchard/ ironwood/
    l00.dat … l31.dat        32 B/node
    subtrees.dat             36 B/entry: root, completing height
```

Every file carries page checksums (`zaino_persistence::pages`).

- Retained nodes: level 0 = every leaf, levels 1..31 = even indices only
  (48 B per commitment). That is exactly the set a frontier's ommers come from,
  so the frontier at any historical size rebuilds with no hashing. The node set
  is the fold's accumulator, not a cache: restart reseeds through the same
  reconstruction serving uses.
- Subtree roots are written by the same fold (an odd-index root never survives
  as an ommer, so it cannot be derived from stored nodes later).
- Commit: height records, nodes and subtree entries appended (each asserted at
  its file's end) → every grown file sealed → `MANIFEST`. The durable carry for
  the next batch is seeded in memory from the chunk just written (no read-back).
- `TreeStateStore::open(fs, path, network)` opens each file at its seal: bytes
  past it dropped, a shorter file (`PageError::Lost`) or a bad tail page
  (`PageError::Tail`) refused, as `zaino_persistence::StoreError`. Nothing else
  is read; the writer then reseeds its carries from ≤ 33 nodes per pool.
- `IndexWriterError` = `Store`, `Inconsistent` (stored nodes will not rebuild
  a frontier) or `Commitment` (a non-canonical note commitment off the wire).
  `new` returns it at boot; inside `run` the same errors panic as
  `tree_state index: <error>`.
- Subtrees are the protocol's 2^16-leaf shards (`SUBTREE_LEVEL`, a constant).
- `committed_files(dir, network)` = every sealed file, for `zainod verify`.
- `heights.idx` and `subtrees.dat` records are fixed arrays with
  `encode`/`decode` beside their golden-bytes tests (`heights.rs`, `subtrees.rs`).

## Non-finalized tier and reorgs

Each non-final block folds into an `imbl` `NonFinalizedTrees`. A commit lands
a contiguous batch starting after `finalized_height` (the last durable height,
inclusive; `None` = empty), reusing non-finalized folds
where they exist and folding the rest (bulk sync skips the non-finalized tier
entirely). A `Reorg` restores the
applied carry from the durable one: no reverse fold, no disk read. Nothing
reorg-able is ever fsynced. See
[`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).

The fold (Merkle hashing) runs under `zaino_sync::compute`, reading note
commitments straight off the sink's shared `Arc<Block>`s; the write runs under
`zaino_sync::blocking`. Both hold their state in a `zaino_sync::Offloaded`; a
panic in either re-raises on the caller (and aborts zainod).

- A batch of blocks folds level by level: each tree level's pairs hash across
  every core (rayon), and the three pools fold concurrently.
- The output is a pure function of (start size, leaves). Any split into batches,
  per-block `apply` included, retains the same nodes and subtree roots
  (`fold::tests` holds it against a naive tree).
