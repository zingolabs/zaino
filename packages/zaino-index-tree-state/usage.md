# zaino-index-tree-state

The commitment-tree index for all three shielded pools (Sapling, Orchard,
Ironwood). Backs `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots`
from one fold over the domain `Block`s the sync pipeline carries. The positional shape it instantiates
is described in
[`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §3.

## Wiring

```rust
use zaino_index_tree_state::{PoolActivations, TreeStateIndexWriter, TreeStateService};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine};

let store = DiskEngine::new(fs).open(&path, &zaino_index_tree_state::schema(network))?;
let index = TreeStateIndexWriter::new(store, batch_bytes)?;
let blocks = block_sink.subscribe(IndexKind::TreeState.name(), queue);
let activations = PoolActivations::from_validator(&validator.get_blockchain_info().await?);
let service = TreeStateService::new(index.published().served(), network, activations);
tokio::spawn(index.published().gate(tips, depth, cancel.child_token()));
tokio::spawn(index.run(blocks));
```

- Generic over the persistence port: `TreeStateIndexWriter<S: Store>` with
  `S::View: SequenceRead`, serving `ReadView<V>` / `TreeStateService<V>`;
  zainod picks `DiskEngine`.
- `TreeStateIndexWriter` runs its own loop over the `zaino_sync::BlockSink`
  subscription (`"tree_state"`): `run` follows it through `Shutdown`,
  publishing through a `zaino_sync::Published<ReadView<V>>`. Each `Apply`
  takes the run of blocks already queued behind it (`Subscription::run`, up to
  `batch_bytes`), folds them as one batch and splits the result back into one
  `Changes` per block. Final blocks (bulk) are staged in
  `zaino_persistence::Tiered`, one commit per `batch_bytes`; once following the
  tip, each `Finalized { height }` commits everything through `height` as it
  arrives. `durable_tip()` = the last committed block, for the producer's start
  and chain check.
- Fallible only at boot: the engine's `open` (`StoreError`) and `new`, which
  reseeds the running frontiers from the store (`IndexWriterError`). `run` is
  infallible: it returns at `Shutdown` and panics on a failed commit or an
  unfoldable block ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- `ReadView` binds the held blocks and the committed snapshot into one publication:
  a request loads it once, so the seam between them cannot move under it.
  `subtree_roots(..)` answers from it with no `synced` gate; tree states are
  read only through `TreeStateService` (`treestate_in` / `latest_in` on a pinned
  view), which adds the gate and the Sapling floor. The index's `Published`
  (`index.published()`) is the only way to get one.
- `activations` = each pool's first height, from the validator's
  `getblockchaininfo` schedule keyed by branch id: Sapling, NU5 (Orchard),
  NU6.3 (Ironwood); an unscheduled upgrade = `None`. zainod reads it once at
  boot (Zaino carries no compiled-in schedule).
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
| `treestate(height)` | `Treestate` with all three pools (`BeforeSapling` below Sapling activation) |
| `latest()` | `treestate` at the highest applied height (non-finalized included) |
| `activations()` | `PoolActivations`, for the transport's wire shape |
| `subtree_roots(pool, start_index, max_entries)` | `Vec<SubtreeRoot>` |

- While `synced` reads `false`, `treestate(height)` still answers any committed
  height (final: the answer never changes); everything else, and any height
  above the committed tip, is `ServeError::Syncing` (gRPC `UNAVAILABLE`): one
  answer, no height or progress. `latest()` on an index holding no blocks is
  `Empty` (also `UNAVAILABLE`). A height with no record is `NotFound`; stored
  nodes that will not rebuild are `Inconsistent` (a fold bug).
- Heights arrive as `Height` (range-checked by the caller).
- Below Sapling activation there is no tree state: `BeforeSapling` (gRPC
  `INVALID_ARGUMENT`, as lightwalletd: zebra's `z_gettreestate` returns no
  Sapling tree there).
- `Treestate` carries every pool as
  `zcash_primitives::merkle_tree::write_commitment_tree` of the real tree
  (`000000` when empty). `zaino-grpc` writes a pool's field only from its
  activation height (`PoolActivations::active`), and `""` below it, matching
  zebra + lightwalletd. An active pool is never `""`: clients map an absent
  field onto an empty tree silently. `final_root` stays unset.
- `subtree_roots`: `max_entries == 0` = to the end; `start_index == count` =
  `Ok(vec![])`. Each root carries its completing block (`BlockRef`: height from
  the subtree entry, hash from that height's record).
- No hashing on the read path: each request is one 48 B height record plus
  ≤ 33 node reads per pool.
- `GetTreeState` by `BlockID.hash` resolves through the block-hash index
  (`zaino-internal-block-hash-to-height`) in `zaino-grpc`; this index answers by
  height and the router confirms the hash it holds there.
- `pin()` → the latest synced publication (`Arc<ReadView>`, one per
  publication); `treestate_in(&view, h)` / `latest_in(&view)` answer from it.
  `is_non_finalized(h)` names the ~1000 heights every synced wallet asks about.
  `zaino-grpc` keys its per-publication memos on the `Arc`: tip tree states and
  whole root lists are framed once per block, not once per wallet.

## Storage

```text
<dir>/
  MANIFEST                   committed tip, every table's seal
  heights.dat                48 B/height: hash, time, three cumulative tree sizes
  sapling/ orchard/ ironwood/
    l00.dat … l31.dat        32 B/node
    subtrees.dat             36 B/entry: root, completing height
```

One `zaino_persistence` store (zainod: `DiskEngine`): `schema(network)` declares the
100 fixed-width sequence tables (`heights`, then per pool `<pool>/l00` …
`<pool>/l31` and `<pool>/subtrees`). The engine owns the manifest, checksums,
crash safety and offline verify; this crate owns the record encodings.

- Retained nodes: level 0 = every leaf, levels 1..31 = even indices only
  (48 B per commitment). That is exactly the set a frontier's ommers come from,
  so the frontier at any historical size rebuilds with no hashing. The node set
  is the fold's accumulator, not a cache: restart reseeds through the same
  reconstruction serving uses.
- Subtree roots are written by the same fold (an odd-index root never survives
  as an ommer, so it cannot be derived from stored nodes later).
- One block = one `Changes`: its height record, the nodes whose last leaf it
  holds and the subtree roots it closes, each asserted at its table's end
  (slot = position). A commit merges the held blocks' `Changes` into one
  `Store::commit`, which fsyncs only the tables that grew (~4 of 100 per
  batch).
- `new` asserts one `heights` record per committed height, then reseeds its
  carries from ≤ 33 nodes per pool.
- `IndexWriterError` = `Inconsistent` (stored nodes will not rebuild a
  frontier) or `Commitment` (a non-canonical note commitment off the wire).
  `new` returns it at boot; inside `run` the same errors panic as
  `tree_state index: <error>`.
- Subtrees are the protocol's 2^16-leaf shards (`SUBTREE_LEVEL`, a constant).
- `schema(network)` = what `zainod verify` passes to
  `PersistenceEngine::verify` for this directory.
- `heights` and `subtrees` records are fixed arrays with
  `encode`/`decode` beside their golden-bytes tests (`heights.rs`, `subtrees.rs`).

## Held blocks and reorgs

Blocks above the durable tip are `zaino_persistence::Tiered`'s: staged
(final, bulk) or applied (tip), keyed exactly as the files, read through the
same `ReadView`. One carry (a frontier per pool) follows the last block held.
A `Reorg` drops every applied block and reseeds the carry off the view's tip
through the same reconstruction a read uses: no reverse fold, no hashing.
Nothing reorg-able is ever fsynced. See
[`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).

The fold (Merkle hashing) runs under `zaino_sync::compute`, reading note
commitments straight off the sink's shared `Arc<Block>`s; the commit runs under
`zaino_sync::blocking`. Both hold their state in a `zaino_sync::Offloaded`; a
panic in either re-raises on the caller (and aborts zainod).

- A run of blocks folds level by level: each tree level's pairs hash across
  every core (rayon), and the three pools fold concurrently. A node lands in
  the `Changes` of the block holding its last leaf.
- The output is a pure function of (start size, leaves). Any split into batches
  retains the same nodes and subtree roots, each under the same block
  (`fold::tests` holds it against a naive tree).
