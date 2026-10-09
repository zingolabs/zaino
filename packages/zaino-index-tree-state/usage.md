# zaino-index-tree-state

The commitment-tree index for all three shielded pools (Sapling, Orchard,
Ironwood). Backs `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots`
from one fold over the domain `Block`s the NFS hands it. The positional shape it instantiates
is described in
[`docs/design/index-data-structures.md`](../../docs/design/index-data-structures.md) §3.

## Wiring

```rust
use zaino_index_tree_state::{TreeStateIndexWriter, FORMAT, TABLES, WRITE_BUFFER};
use zaino_persistence::{DiskEngine, IndexKind, PersistenceEngine, Schema};

let schema = Schema::new(IndexKind::TreeState, FORMAT, network, TABLES);
let store = DiskEngine::new(fs, LsmConfig::default()).open(&path, &schema, WRITE_BUFFER)?;
let writer = TreeStateIndexWriter::new(store);
let handle = writer.handle();
let blocks = follower.subscribe(IndexKind::TreeState, handle.tip(), queue_bytes);
nfs.add(IndexKind::TreeState, handle);
tokio::spawn(writer.run(blocks));
```

- Generic over the persistence port: `TreeStateIndexWriter<S: Store>` with
  `S::View: SequenceRead`; zainod picks `DiskEngine`.
- `run` follows the final stream (`"tree_state"`, from `FinalFollower`) run by
  run ([the writer shape](../zaino-sync/usage.md#writer-loop)): the blocks not
  held are folded as one [`fold_run`](#fold) onto `staged()` into one delta per
  block, each then `zaino_sync::apply`d. Commits: a `Finalized` block, a full
  buffer (`WRITE_BUFFER` = 8 MiB), `Shutdown`. `handle()` = the `IndexHandle`
  the NFS reads (committed view, durable tip).
- Fallible only at boot, in the engine's `open` (`StoreError`). `new` asserts
  one record per committed height. `run` panics on a failed commit or an
  unfoldable block (`tree_state index: <FoldError>`,
  [Failure](../zaino-sync/usage.md#failure-panic-never-err)).
- `PoolActivations` = each pool's first height, from the validator's
  `getblockchaininfo` schedule keyed by branch id: Sapling, NU5 (Orchard),
  NU6.3 (Ironwood); an unscheduled upgrade = `None`. zainod reads it once at
  boot (no compiled-in schedule) into the NFS's `ChainParams`.
- zainod builds it only when `index.tree_state.enabled`; disabled = the three
  methods `UNIMPLEMENTED`.

## Serving

Routes read through one snapshot per request (`snap.views().tree_state()`,
`None` = disabled), at heights `≤ snap.tip()`:

| Reader method | Returns |
|---|---|
| `treestate(height)` | `Treestate` with all three pools; `NotFound` with no record, `Inconsistent` if stored nodes will not rebuild (a fold bug) |
| `subtree_roots(pool, start_index, max_entries)` | `Vec<SubtreeRoot>` |
| `is_non_finalized(height)` | `height` in the snapshot's layer above the committed files |

- The route (`zaino-grpc`) adds what the reader does not know: past the
  snapshot tip = `NOT_FOUND`; below Sapling activation = `INVALID_ARGUMENT`
  (lightwalletd: zebra's `z_gettreestate` returns no Sapling tree there);
  `GetLatestTreeState` = `treestate(snap.tip())`; subtree roots completing above
  the tip are left out; `TreeState.network` = the declared network
  (`snap.params().network`).
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
- `is_non_finalized(h)` names the ~1000 heights every synced wallet asks about.
  `zaino-grpc` keys its memos on the snapshot's `Arc`: layer tree states, the
  tip and whole root lists are framed once per snapshot, not once per wallet.

## Storage

```text
<dir>/
  MANIFEST                   committed tip, every table's seal
  heights.dat                48 B/height: hash, time, three cumulative tree sizes
  sapling/ orchard/ ironwood/
    l00.dat … l31.dat        32 B/node
    subtrees.dat             36 B/entry: root, completing height
```

One `zaino_persistence` store (zainod: `DiskEngine`): `TABLES` declares the
100 fixed-width sequence tables (`heights`, then per pool `<pool>/l00` …
`<pool>/l31` and `<pool>/subtrees`), all `const`. The engine owns the manifest,
checksums, crash safety and offline verify; this crate owns the record
encodings.

- Retained nodes: level 0 = every leaf, levels 1..31 = even indices only
  (48 B per commitment). That is exactly the set a frontier's ommers come from,
  so the frontier at any historical size rebuilds with no hashing. The node set
  is the fold's accumulator, not a cache: every fold reads its starting
  frontiers through the same reconstruction serving uses.
- Subtree roots are written by the same fold (an odd-index root never survives
  as an ommer, so it cannot be derived from stored nodes later).
- One block = one delta (`BlockChanges`): its height record, the nodes whose last
  leaf it holds and the subtree roots it closes, each asserted at its table's
  end (slot = position). One `Store::commit` writes every buffered block's
  `BlockChanges` and fsyncs only the tables that grew (~4 of 100 per
  commit).
- `new` asserts one `heights` record per committed height.
- Subtrees are the protocol's 2^16-leaf shards (`SUBTREE_LEVEL`, a constant).
- `Schema::new(IndexKind::TreeState, FORMAT, network, TABLES)` = what
  `zainod verify` passes to `PersistenceEngine::verify` for this directory.
- `heights` and `subtrees` records are fixed arrays with
  `encode`/`decode` beside their golden-bytes tests (`heights.rs`, `subtrees.rs`).

## Fold

```rust
use zaino_index_tree_state::{fold, FoldError, TreeStateReader};

let parent = TreeStateReader::new(view);           // any `V: SequenceRead` over these tables
let mut out = store.changes(block.at());           // or the parent layer's `changes`
fold(&parent, &block, &mut out)?;                   // block = next above parent's tip
```

- `fold` lives in `writer.rs`, beside the writer loop and its crate-internal
  `fold_run(parent, blocks, out: &mut [BlockChanges])` (a contiguous run, one
  caller-opened delta per block).
- The parent's state (tree sizes from its tip record, each pool's frontier,
  each table's length) is read through the reader; nothing is carried between
  calls, so reorg and restart need no step. A delta opened for another block,
  or a block off the parent tip, panics naming the index.
- `fold_run` hashes the whole run level by level: one `combine_pairs` per tree
  level across every block (the node types split a wide level across every
  core), the three pools concurrently. A node or subtree root lands in the
  delta of the block holding its last leaf, so any split into runs yields the
  same deltas (`writer::tests`, against a naive tree and block by block).
  `fold` = a run of one.
- `FoldError` = `Inconsistent` (the parent's nodes will not rebuild a frontier)
  or `Commitment` (a non-canonical note commitment off the wire, naming its
  block); every leaf is decoded before any hashing.

## Non-final blocks and reorgs

Blocks above the durable root are `zaino-nfs`'s: one `Overlay` per block, keyed
exactly as the files, read through the same `TreeStateReader` over a
`OverlayView`. A reorg moves the snapshot to another node; a block folds onto
its parent node's frontiers: no reverse fold. Nothing reorg-able is ever
fsynced. See [`docs/design/nfs.md`](../../docs/design/nfs.md).

The writer's `fold_run`, apply and commit run in one `zaino_sync::blocking`
hop per run, reading note commitments straight off the stream's shared
`Arc<Block>`s (the pools' hashing on the rayon pool via `rayon::join`). A panic
re-raises on the caller (and aborts zainod).
