# zaino-index-tree-state

The commitment-tree index for the three shielded pools (Sapling, Orchard,
Ironwood). It backs `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots`
at any height, past or present, without hashing on the read path.

## What it stores

Fixed-width sequences only, where a record's slot is its key, so no key is
stored:

```text
<dir>/
  MANIFEST                   durable tip, every table's seal (the engine's)
  heights.dat                48 B per height: hash, time, three cumulative tree sizes
  sapling/ orchard/ ironwood/
    l00.dat … l31.dat        32 B per retained node, one file per tree level
    subtrees.dat             36 B per completed 2^16-leaf subtree: root, completing height
```

`TABLES` declares the 100 sequences (`heights`, then per pool `<pool>/l00` …
`<pool>/l31` and `<pool>/subtrees`).

**Retained nodes.** A textbook incremental tree keeps only its latest frontier,
and a full tree keeps 2N nodes. This index keeps every leaf (level 0) and every
even-index internal node (levels 1–31), about 48 bytes per commitment. That is
exactly the set any past frontier is built from: an ommer is always the left
sibling of a right child, so its index is always even. The frontier at any
historical tree size can then be read back directly, never rebuilt by hashing.

**Subtree roots** come from the same fold. Each time a pool completes a
2^16-leaf subtree (`SUBTREE_LEVEL`), its root and completing height go into
that subtree's slot. The fold has to write them as they close: a subtree root
at an odd index is never retained as an ommer, so it cannot be derived from the
stored nodes later.

**One block contributes** its `heights` record, the nodes whose last leaf it
holds, and the subtree roots it closes. Each one lands at its table's end
(slot = position, asserted).

## How a read finds the tree

`treestate(h)` reads `heights[h]` for the block hash, time and each pool's tree
size `s`. Then, per pool, with `p = s − 1`, it reads the leaf `l00[p]` plus one
stored node per set bit of `p`. That is at most 33 reads of 32 bytes, and no
hashing. `subtree_roots(pool, i, n)` is a seek to slot `i`; each root's block
hash comes from its completing height's `heights` record.

## Wiring

```rust,ignore
use zaino_index_tree_state::{TreeStateIndexWriter, FORMAT, TABLES, WRITE_BUFFER};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};

let schema = Schema::new(IndexKind::TreeState, FORMAT, network, TABLES);
let store = DiskEngine::new(fs, LsmConfig::default()).open(&path, &schema, WRITE_BUFFER)?;
let writer = TreeStateIndexWriter::new(store);
let handle = writer.handle();
let blocks = follower.subscribe(IndexKind::TreeState, handle.view().tip(), queue_bytes);
nfs.add(IndexKind::TreeState, handle);
tokio::spawn(writer.run(blocks));
```

- Generic over the persistence port: `TreeStateIndexWriter<S: Store>` with
  `S::View: SequenceRead`. zainod picks `DiskEngine`.
- `run` follows the finalized stream ([the writer shape](../zaino-sync/usage.md#writer-loop)).
  It folds each run's not-yet-held blocks together, one delta per block, then
  `zaino_sync::apply`s each. It commits on a `FinalizedTip` block, a full buffer
  (`WRITE_BUFFER` = 16 MiB) and `Shutdown`.
- Fallible only at boot (`StoreError` from `open`). `new` asserts one `heights`
  record per durable height. `run` panics on a failed commit or an unfoldable
  block ([Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- `PoolActivations::new(&LocalNetwork)` gives each pool's first height:
  Sapling, NU5 (Orchard), NU6.3 (Ironwood), or `None` when unscheduled.
- zainod builds it only when `index.tree_state.enabled`. Disabled, the three
  methods answer `UNIMPLEMENTED`.

## Fold

```rust,ignore
use zaino_index_tree_state::{fold, FoldError, TreeStateReader};

let parent = TreeStateReader::new(view);  // any `V: SequenceRead` over these tables
let mut out = store.changes(block.at());  // or the parent layer's `changes`
fold(&parent, &block, &mut out)?;          // block = next above parent's tip
```

- The stored nodes are the fold's only state. Each fold reads its starting
  frontiers back through the same reader serving uses, so nothing is carried
  between calls and restarts or reorgs need no special step.
- A delta opened for another block, or a block that does not extend the
  parent's tip, panics naming the index.
- The writer folds a whole run at once (crate-internal `fold_run`): one
  `combine_pairs` per tree level across every block, the three pools
  concurrently. A node lands in the delta of the block holding its last leaf,
  so any split into runs gives the same deltas. `fold` is a run of one.
- `FoldError::Inconsistent`: the parent's nodes do not rebuild a frontier (a
  fold bug). `FoldError::Commitment`: a non-canonical note commitment off the
  wire, naming its block. Every leaf is decoded before any hashing.

## Serving

Routes read through one `GrpcSnapshot` per request (`at.views().tree_state()`,
`None` = disabled), at heights `≤ at.answers_through(IndexKind::TreeState)`:

| Reader method | Returns |
|---|---|
| `treestate(height)` | `Treestate` with all three pools; `NotFound` with no record, `Inconsistent` if stored nodes will not rebuild |
| `subtree_roots(pool, start_index, max_entries)` | `Vec<SubtreeRoot>`; `max_entries == 0` = to the end, `start_index == count` = empty |
| `is_above_durable(height)` | `height` is in the NFS layer above the durable files |

`zaino-grpc` adds what the reader does not know:

- Past `answers_through` is `NOT_FOUND`. Below Sapling activation every pool is
  `""`, where lightwalletd errors: the Android SDK asks for the height just
  below activation (ZA#1422).
- `GetLatestTreeState` = `treestate(at.tip())`. Subtree roots completing
  above the tip are left out.
- Each pool's tree is `write_commitment_tree` of the real tree (`000000` when
  empty). A pool's field is written only from its activation height
  (`PoolActivations::active`), and `""` below it, matching zebra and
  lightwalletd. An active pool is never `""`, because clients silently read an
  absent field as an empty tree.
- By `BlockID.hash`, the block-hash index
  ([`zaino-internal-block-hash-to-height`](../zaino-internal-block-hash-to-height/usage.md))
  names a height, and the route checks `Treestate.block_hash` matches.
- `is_above_durable` marks the ~1000 heights every synced wallet asks about.
  `zaino-grpc` memoizes those answers per NFS publish, not per wallet.

Blocks above the durable tip are `zaino-nfs` layers, keyed exactly as the
files and read through the same `TreeStateReader` over an `OverlayView`. A
reorg moves the served tip to another layer, so nothing reorgable ever reaches
disk.

## Storage

- Manifest, checksums, crash safety and `zainod verify` belong to the engine
  ([`zaino-persistence`](../zaino-persistence/usage.md)). This crate owns the
  record encodings: `heights.rs` and `subtrees.rs` each pair `encode`/`decode`
  with a golden-bytes test.
- One commit writes every buffered block's delta and seals only the tables that
  grew.
