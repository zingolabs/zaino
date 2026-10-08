# zaino-nfs: the tip overlay

Status: **implemented** (2026-10-08). See [pipeline.md](pipeline.md) for where this fits.

The NFS (non-finalized state) serves the blocks that are not durable yet. It folds each one in
RAM over the indexes' committed views, publishes a snapshot at once, and drops the node once every
index has committed that height. It never writes to disk and never sends anything to the writers:
final blocks reach them through the final path ([data-sink.md](data-sink.md)), where they are
folded a second time.

```text
 VerifiedChain ──────┐                    ┌─▶ Fetch ───▶ zaino_sync::fetch(.., Tip) ─▶ Checked
 checked bodies ─────┤                    │
 fold results ───────┼─▶ NfsCore::step ───┼─▶ Fold ────▶ fold_block on rayon ─▶ Node in graph
 IndexHandle commits ┘   (pure: no I/O)   │
   (durable, serving)                     └─▶ Publish ─▶ watch<Option<Indexed>> ─▶ zaino-snapshot
```

## Public API

```rust
impl<S: ChainDataSource, V: SequenceRead + MapRead> Nfs<S, V> {
    pub fn new(chain: watch::Receiver<Option<Arc<VerifiedChain>>>, balancer: TrafficBalancer<S>,
               params: ChainParams, depth: ReorgDepth, lookahead: NonZeroUsize) -> Self;
    pub fn add(&mut self, kind: IndexKind, index: IndexHandle<V>);  // writer.handle()
    pub fn indexed(&self) -> Published<V>;                         // watch<Option<Arc<Indexed<V>>>>
    pub async fn run(self, cancel: CancellationToken) -> Result<(), NfsError>;
}

pub enum NfsError { Fold(FoldError), ChainGone, IndexGone(&'static str) }

impl<V: View> Indexed<V> {
    pub fn chain(&self) -> &Arc<VerifiedChain>;
    pub fn served(&self) -> &At<V>;                      // deepest folded best node, else durable
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>>; // any folded node, side branches included
    pub fn durable(&self) -> impl Iterator<Item = (IndexKind, Option<BlockRef>)>;
}
impl<V> At<V> {
    pub fn tip(&self) -> BlockRef;
    pub fn branch(&self) -> Branch;                      // Best | Side { from }
    pub fn views(&self) -> &Views<V>;                    // per index: committed view + node layer
}
impl<V> Views<V> {
    pub fn compact_block(&self) -> Option<CompactBlockReader<LayeredView<V>>>; // None: disabled,
    pub fn tree_state(&self) -> Option<TreeStateReader<LayeredView<V>>>;       // or not serving
    pub fn block_hash(&self) -> Option<BlockHashReader<LayeredView<V>>>;
    pub fn transparent_address(&self) -> Option<TransparentAddressReader<LayeredView<V>>>;
    pub fn syncing(&self, kind: IndexKind) -> bool;      // enabled, not serving (UNAVAILABLE)
}
```

## Rules

1. **Serving comes from each index.** On every chain change and every commit, the driver asks
   each `IndexHandle::serving(best, 2 · depth)`. A non-serving index is `syncing`: not folded,
   not served. During a first sync nothing serves, so the NFS fetches and folds nothing.
2. **What gets folded.** Every best-chain block above the lowest durable tip among serving
   indexes. A node at height `h` folds each serving index durable below `h`, in `fold_block`
   order (value-balance first: its fees feed compact-block; value-balance durable at `h` = fees
   from its committed view).
3. **Prune.** A best-chain node goes once every serving index has committed its height. A side
   node goes when the header chain drops its fork.
4. **Serving set changed → refold.** Every best node's block goes back to `ready`, the graph is
   dropped and refolded (about 1 ms per node). The served tip is held until the refold reaches
   it again.
5. **Served tip** = the deepest folded best-chain node, else the committed views alone; nothing
   durable and nothing serving = withdrawn (`None`). Every route answers at heights `≤` it, so
   `GetLatestBlock`, `GetBlockRange` and `GetTreeState` agree.
6. **One state.** The driver reloads every committed view only when it tells the core the index
   states, so folds and snapshots pair layers with exactly the durable tips the core knows.

## Core

```rust
pub(crate) enum Input<F> {
    Chain(Arc<VerifiedChain>),
    Body(Checked),                                   // stale = ignored
    Folded { at: BlockRef, covers: Indexes, folded: Arc<F> }, // another covers (stale) = ignored
    Indexes(Vec<(Option<BlockRef>, bool)>),          // every index: (durable tip, serving)
}

pub(crate) enum Output<F> {
    Fetch { at: BlockRef, record: Record },          // one per wanted block, until its body
    Abandon(BlockRef),                               // no longer wanted: fetch dropped
    Fold { at: BlockRef, parent: Option<Arc<F>>, block: Arc<Block>, covers: Indexes },
    Publish(Option<SnapshotTip<F>>),
}
```

The driver is one task: `select!` over the chain watch, fetch tasks, fold results and each
index's `changed()`, then it executes the outputs. Folds run on the compute pool, never on the
async loop.

## A block's life

```text
header verified ─▶ on best, above the root ─▶ Fetch ─▶ Checked ─▶ Fold (parent node, or the
   committed views) ─▶ Node ─▶ Publish
   ─▶ … final ─▶ (final path: follower → writers fold + commit) ─▶ every serving index's
       durable tip ≥ h ─▶ node pruned
```

A reorg (best moves to a branch forking at F):

```text
before: … F ─ a1 ─ a2 ─ a3   (served a3)
after:  … F ─ b1 ─ b2        (b1, b2 fetched + folded from F)
        served: a3 → F (at once) → b1 → b2 (as each folds)
        a1–a3 stay until the header chain drops their fork; switching back costs nothing
```

## Invariants

| ID  | Invariant                                                                            |
| --- | ------------------------------------------------------------------------------------ |
| N1  | every node's block = its own header + merkle root, on the best chain or a held side  |
| N2  | nodes above the root, on a held parent, folding serving indexes only                 |
| N3  | durable tips never move back                                                         |
| N4  | served tip = deepest folded best node, else the root (unless held by a refold)       |
| N6  | every index read through a snapshot = that index folded from genesis along best      |

## Tests

- **Core model** (`core/model.rs`): random chain evolutions (extend, reorg, retreat, revive,
  finalize), late and stale bodies and folds, writers committing the final prefix after random
  delays, restarts with a wiped index. The oracle folds from genesis along each block's path;
  every fold, node and publish must match it. `check()` after every step.
- **Fire drills** (`core/fire_drills.rs`): one planted bug per `check()` assertion and
  precondition; stale folds ignored; a serving change refolds and holds the served tip.
- **End to end** (`tests.rs`): `FinalFollower` + the five real writers + the NFS over `SimFs`,
  mock validators (one lying): reorgs, finality, a restart, an index enabled late (`syncing`
  until it serves, served tip never back), a writer gone.

The persistence types the overlay builds on (`Layer`, `LayeredView`, `Store::staged`) are in
[persistence-engine.md §5](persistence-engine.md#5-layers-and-writers). How requests read
snapshots is in [global-snapshot.md](global-snapshot.md).
