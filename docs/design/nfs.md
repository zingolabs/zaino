# zaino-nfs: one non-finalized state, folds, one snapshot

Status: **target design, for review** (2026-10-07). §4 (the port) is implemented; the rest is
not yet. The current code is described by [non-finalized-state.md](non-finalized-state.md), [data-sink.md](data-sink.md)
and [sync.md](sync.md); those are rewritten as this lands.

Builds on [verified-chain.md](verified-chain.md) (the header chain decides best and final) and
[persistence-engine.md](persistence-engine.md) (the port).

## 1. Why

"The chain above the final tip" lives in four places today: the header chain's best path, the
producer's held blocks, every index's RAM tier (`Tiered::apply`/`reorg`), and every index's
serving gate. A reorg is replayed by the producer and re-applied by five indexes. A gRPC request
pins one index's view, so `GetLatestBlock`, `GetBlockRange` and `GetTreeState` agree only because
each index is gated separately (test plan R12).

The end state has one of each:

| Concept                             | One place                                                 |
| ----------------------------------- | --------------------------------------------------------- |
| which chain is best, what is final  | `zaino-header-chain` (`VerifiedChain`)                    |
| index state as of a non-final block | `zaino-nfs`: one `imbl` node per block                    |
| how a block changes an index        | that index's `fold`                                       |
| what a request reads                | one `Snapshot`                                            |
| how data becomes durable            | the final stream (`BlockSink`) → writer → `Store::commit` |

## 2. The whole pipeline

```text
                     trusted validators, peers
                                │ headers                     │ blocks (checked: hash + merkle)
                                ▼                             ▼
  zaino-header-chain ── VerifiedChain ──▶ zaino-nfs ◀──── fetch (any source)
     (best, final)      (watch)            │
                                           │  imbl graph, one Node per block above the root
                                           │    Node { block, folded: per-index Changes,
                                           │           layers: per-index Layer }
                                           │
                     ┌─────────────────────┼──────────────────────────┐
                     ▼                     ▼                          ▼
              Snapshot (ArcSwap)     BlockSink: Final{block,      fetch wants
                     │                folded: Option}             (bulk + tip)
                     │                     │
                     │        ┌────────────┼─────────────┬──────────────┐
                     │        ▼            ▼             ▼              ▼
                     │   value_balance  compact_block  tree_state  transparent / block_hash
                     │    writer ──fees──▶ writer        writer        writers
                     │        │ (bulk only) │             │              │
                     │        └────── Store::apply → Store::commit (one fsync per batch) ──┐
                     │                                                                     │
                     │◀──────────────────── durable tips (acks) ◀──────────────────────────┘
                     ▼
  zaino-grpc:  let snap = nfs.snapshot()?;   CompactBlockReader::at(&snap)  TreeStateReader::at(&snap) …
```

- Below the NFS root (bulk sync): the NFS fetches final blocks and sends them **unfolded**; each
  writer folds, buffers and commits.
- Above the root (the tip): the NFS fetches, checks and **folds** each block as it joins the
  verified best; when it turns final the NFS sends it **folded** and every writer just commits.
- One sender (`zaino-nfs`), one stream, every step final: there is no `Reorg` and no `Finalized`
  step anymore.

## 3. Vocabulary

| Term             | Meaning                                                                                  |
| ---------------- | ---------------------------------------------------------------------------------------- |
| **fold**         | `fold(parent: &Reader, block, inputs) -> Changes`: one index's state transition, pure    |
| **Changes**      | the port's write unit: one buffer per table, shaped by the schema (unchanged)            |
| **Layer**        | one index's non-final data as of one block: `imbl` per table, = parent's + own `Changes` |
| **LayeredView**  | a `Layer` over a store's committed `View`; implements `SequenceRead` + `MapRead`         |
| **Node**         | one non-final block in the graph: its body, its `Folded`, its `Layers`                   |
| **root**         | the common durable tip of every enabled store; nodes exist only above it                 |
| **Snapshot**     | one node's layers + every store's committed view, at one served tip                      |
| **final stream** | `BlockSink<Final>`: every block exactly once, in height order, final                     |

## 4. Persistence port (target)

### Store: buffer, then commit atomically

```rust
pub trait Store: Send + 'static {
    type View: View;

    fn schema(&self) -> &Schema;
    fn path(&self) -> &Path;

    /// Final data, buffered (not durable, not in `view()`)
    fn apply(&mut self, changes: Changes);
    fn buffered_bytes(&self) -> usize;

    /// Every buffer → disk, one atomic commit (one fsync); `Err` poisons the store
    fn commit(&mut self) -> Result<(), StoreError>;

    /// Committed only: what serving pins (a crash never takes back what a reader saw)
    fn view(&self) -> Self::View;

    /// Committed + buffered: what a bulk fold reads its parent through
    fn staged(&self) -> LayeredView<Self::View>;
}
```

- `apply` asserts the next tip is above the last applied one, the `Changes` match the schema and
  no map key is buffered twice, before buffering anything.
- `commit` with nothing buffered = `Ok`, nothing written (a writer's final commit is
  unconditional).
- `PersistenceEngine`, `View`, `SequenceRead`, `MapRead`, `Schema`, `Changes`, `Width` are
  unchanged.
- `Tiered` is deleted: its non-final half becomes the NFS's layers, its staging becomes
  `Store::apply`.

### Layer and LayeredView: the `imbl` core

```rust
/// One index's non-final data as of one block (clone = O(tables) pointer copies)
#[derive(Clone)]
pub struct Layer {
    deltas: imbl::Vector<Arc<Delta>>,                    // per Changes absorbed: tip, its share
    sequences: Vec<imbl::Vector<Bytes>>,                 // per SequenceId: records past durable
    maps: Vec<imbl::OrdMap<Bytes, Bytes>>,               // per MapId: inserts above durable
}

impl Layer {
    pub fn empty(schema: &Schema) -> Self;
    pub fn tip(&self) -> Option<BlockRef>;                // last block absorbed
    pub fn with(&self, changes: &Changes) -> Self;        // parent + changes, structural sharing
    pub fn rebase(&self, durable: &impl View) -> Self;    // drop what `durable` now holds
}

/// A layer over a committed view: layer first, then disk
#[derive(Clone)]
pub struct LayeredView<V> { durable: V, layer: Layer }

impl<V: View> LayeredView<V> {
    pub fn new(durable: V, layer: Layer) -> Self;         // panics: layer not above durable's tip
    pub fn durable(&self) -> &V;                          // the seam (readers' finalized tip)
}
impl<V: View> View for LayeredView<V> { /* tip = layer tip, else durable tip */ }
impl<V: SequenceRead> SequenceRead for LayeredView<V> { /* position ≥ durable len → layer */ }
impl<V: MapRead> MapRead for LayeredView<V> { /* layer key → layer, else disk; ranges merged */ }
```

- A store's buffer and the NFS's nodes are the same type: `Store::staged()` = committed view +
  the buffer's `Layer`.
- `deltas` (not a bare `tip`): `rebase` must know how many records each table drops, and a
  `Changes` carries no positions; each block's share is that count plus its keys. `rebase` panics
  when durable's tip is past the layer or not one of its blocks (another branch).
- Conformance (`history` + `contract`, any engine): apply / commit / crash / reopen, and
  `view() == committed prefix`, `staged() == committed + buffered` after every step; `Layer`
  steps (`with`, `rebase`) checked against a naive `BTreeMap` overlay.

## 5. Index crates (target shape)

Every index crate has the same four parts. Using compact-block:

```text
zaino-index-compact-block/src/
  lib.rs       schema(network), BLOCKS, FORMAT, re-exports
  fold.rs      pub fn fold(parent, block, fees) -> Changes       (pure; golden tests beside it)
  reader.rs    pub struct CompactBlockReader<V>                    (typed reads over LayeredView<V>)
  writer.rs    pub struct CompactBlockWriter<S: Store>             (final stream → apply → commit)
  build.rs, project.rs                                            (wire encoding, unchanged)
```

```rust
// fold.rs: parent state is read, never carried
pub fn fold<V: SequenceRead>(
    parent: &CompactBlockReader<V>,
    block: &Block,
    fees: &BlockFees,
) -> Result<Changes, TreeSizeOutOfRange> {
    // asserts `block` extends `parent.tip()` (a wrong parent mis-sizes every later record)
    let sizes = parent.tip_sizes().advance(block)?;              // parent record's chainMetadata
    let mut changes = Changes::new(at, &schema(parent.network()));
    changes.append(BLOCKS, &encode_compact_block(block, fees, &sizes));
    Ok(changes)
}

// reader.rs: any view (a committed view, or a `LayeredView` over one)
pub struct CompactBlockReader<V> { view: V, network: NetworkType }
impl<V: SequenceRead> CompactBlockReader<V> {
    pub fn new(view: V, network: NetworkType) -> Self;
    pub fn at(snapshot: &Snapshot<V>) -> Result<Self, ReadError>;     // wave 3: Unimplemented | Syncing
    pub fn tip(&self) -> Option<BlockRef>;
    pub fn block(&self, at: Height) -> Option<Bytes>;
    pub fn range(&self, first: Height, last: Height, budget: usize) -> (Vec<Bytes>, Height);
    pub(crate) fn tip_sizes(&self) -> TreeSizes;
}
```

| Index               | fold                          | inputs      | outputs                | reader (public)                                   |
| ------------------- | ----------------------------- | ----------- | ---------------------- | ------------------------------------------------- |
| value-balance       | resolve prevouts, fee per tx  | —           | `Changes`, `BlockFees` | (internal: fees only)                             |
| compact-block       | encode record + tree sizes    | `BlockFees` | `Changes`              | `tip`, `block`, `range`                           |
| block-hash          | hash → height row (no parent) | `network`   | `Changes`              | `height_of(&BlockHash)`                           |
| tree-state          | append commitments, frontiers | —           | `Changes`              | `treestate(h)`, `subtree_roots(pool, start, max)` |
| transparent-address | receives, spends (outpoint)   | —           | `Changes`              | `utxos`, `balance(s)`, `transactions`             |

- **Fold order = the dependency graph**, written once in `zaino-nfs::fold_block`: value-balance
  first (its fees feed compact-block), then the rest.
- Fallible folds return `Result`: value-balance `FoldError` (missing prevout, negative fee,
  overflow), compact-block `TreeSizeOutOfRange` (#549), tree-state `FoldError` (a non-canonical
  note commitment, or parent nodes that will not rebuild a frontier). A compact-block fold onto a
  non-parent panics (value-balance's parent may be any later state: insert only).
- Runs: tree-state exports `fold_run(parent, blocks) -> Result<Vec<Changes>, FoldError>` (one
  batched Merkle hashing per run, split per block); value-balance has a crate-internal `fold_run`
  (one prevout probe per run). `fold` = a run of one. Bulk sync uses runs; the NFS folds one block.
- transparent-address's fold is a lookup-free projection (spends keyed by outpoint, unspent =
  a read-time miss in `spent`); its parent reader supplies only the network.
- Writers: one loop each, no reorg, no tiers, no gate:

```rust
pub async fn run(mut self, mut blocks: Subscription<Final>) {
    while let Some(Final { block, folded }) = blocks.next().await {
        let changes = match folded {
            Some(folded) => folded.compact_block.clone().expect("enabled"),   // tip: folded by the NFS
            None => fold(&self.reader(), &block, &self.fees.next().await),    // bulk: fold here
        };
        self.store.apply(changes);
        if self.store.buffered_bytes() >= self.batch || folded.is_some() {
            self.commit().await;                                             // offloaded, one fsync
        }
    }
    self.commit().await;
}
```

## 6. zaino-nfs

```text
zaino-nfs/src/
  lib.rs        Nfs (driver), NfsHandle, Snapshot, re-exports
  core.rs       NfsCore: pure state machine (no I/O, time as input), check()
  graph.rs      Node, Folded, Layers; imbl::HashMap<BlockHash, Arc<Node>>
  fold.rs       fold_block: the fold order, the one place indexes meet
  fetch.rs      wants, hedging, blame (moved from zaino-sync's ProducerCore)
  snapshot.rs   Snapshot<V>, Enabled
  core/model.rs, core/fire_drills.rs
```

```rust
pub struct Node { at: BlockRef, parent: BlockHash, block: Arc<Block>, folded: Arc<Folded> }

pub struct Folded {
    fees: Arc<BlockFees>,
    value_balance: Changes,
    compact_block: Option<Changes>,
    block_hash: Option<Changes>,
    tree_state: Option<Changes>,
    transparent_address: Option<Changes>,
    layers: Layers,                          // per index: parent's layer.with(own changes)
}

pub enum Input {
    Chain(Arc<VerifiedChain>),
    Body { at: BlockRef, from: SourceId, result: Result<Arc<Block>, Misanswer> },
    Folded { at: BlockRef, folded: Arc<Folded> },
    Durable { index: IndexKind, tip: Option<BlockRef> },
    Tick,
}

pub enum Output {
    Fetch { at: BlockRef, from: SourceId },
    Fold { at: BlockRef, parent: Arc<Snapshot<V>>, block: Arc<Block> },   // run on the blocking pool
    Send(Final),                                                          // to the BlockSink
    Publish(Arc<Snapshot<V>>),
}

impl NfsCore {
    pub fn step(&mut self, input: Input, now: Instant) -> Vec<Output>;
    pub fn check(&self);                                                  // N1–N6, named panics
}
```

A block's life:

```text
header verified ─▶ on best? ─▶ fetch (any source) ─▶ checked (hash_at + merkle)
   ─▶ below root+final? ── yes ─▶ Send(Final{block, folded: None})            (bulk)
                       └─ no ──▶ Fold (parent = its parent node) ─▶ node joins graph
                                  ─▶ Publish(snapshot at deepest folded best node)
   ─▶ final ─▶ Send(Final{block, folded: Some}) ─▶ every store acks ─▶ root advances,
       node pruned, live layers rebased
```

A reorg (best moves to a branch forking at F):

```text
before: root … F ─ a1 ─ a2 ─ a3   (snapshot at a3)
after:  root … F ─ b1 ─ b2        (b1, b2 fetched + folded on demand, from F's node)
        snapshot: a3 → F (the moment best moves) → b1 → b2 (as each folds)
        a1–a3 stay until pruned with the header chain's side branches; switching back = free
```

### Snapshot

```rust
pub struct Snapshot<V> {
    verified: Arc<VerifiedChain>,
    tip: BlockRef,                               // folded, on the verified best
    params: ChainParams,                         // network, pool activations
    compact_block: Option<LayeredView<V>>,       // None = disabled or still bulk-syncing
    block_hash: Option<LayeredView<V>>,
    tree_state: Option<LayeredView<V>>,
    transparent_address: Option<LayeredView<V>>,
}

pub struct NfsHandle<V> { current: Arc<ArcSwap<Option<Snapshot<V>>>>, changed: watch::Receiver<u64> }
impl<V> NfsHandle<V> {
    pub fn snapshot(&self) -> Option<Arc<Snapshot<V>>>;   // one atomic load
    pub async fn changed(&mut self);                      // GetMempoolStream ends on a new block
}
```

- Every request or stream pins one snapshot for its life (nodes through `Arc`, disk through the
  pinned view): a commit or reorg mid-stream cannot move what it reads.
- `GetLatestBlock` = `snap.tip`; every RPC answers at heights `≤ snap.tip`: they agree by
  construction (R12 closed, W2 "already servable" holds by definition).
- Bulk sync: no nodes; snapshot = committed views at the lowest durable tip of the enabled
  indexes.
- An index enabled later bulk-syncs alone; its slot is `None` (routes answer syncing) until its
  durable tip reaches the root.

## 7. gRPC and zainod

```rust
// zaino-grpc: routes hold the handle, not per-index views
pub struct Routes<V> { nfs: NfsHandle<V>, chain: Arc<ChainView>, validators: TrafficBalancer, … }

async fn get_block_range(&self, req: BlockRange) -> Result<Stream, Status> {
    let snap = self.nfs.snapshot().ok_or_else(syncing)?;
    let reader = CompactBlockReader::at(&snap).map_err(to_status)?;
    let (first, last) = req.bounds(snap.tip())?;
    Ok(stream_range(reader, first, last))            // the reader (and so the snapshot) moves in
}
```

```rust
// zainod boot (the approved shape)
let engine = DiskEngine::new(fs);
let mut nfs = Nfs::new(header_sync.subscribe(), fetch_sources, config.nfs());

// compact-block folds after value-balance (its fees); nfs.subscribe(kind) = enable + final stream
if let Some(cb) = config.index.compact_block.enabled() {
    let fees = ValueBalanceWriter::open(&engine, cb)?.start(&mut tasks, nfs.subscribe(IndexKind::ValueBalance));
    CompactBlockWriter::open(&engine, cb)?.start(&mut tasks, nfs.subscribe(IndexKind::CompactBlock), fees);
}
if let Some(ts) = config.index.tree_state.enabled() {
    TreeStateWriter::open(&engine, ts)?.start(&mut tasks, nfs.subscribe(IndexKind::TreeState));
}
// block_hash, transparent_address: same
let snapshots = nfs.start(&mut tasks);
GrpcService::new(Routes::new(snapshots, chain_view, validators), &config.serve).bind().await?.start(&mut tasks);
```

## 8. Deleted

- `zaino_persistence::Tiered` (→ `Store::apply`/`staged` + `Layer`/`LayeredView`)
- `zaino-sync`: `Producer`, `ProducerCore` (fetch moves to `zaino-nfs::fetch`), `Step::Reorg`,
  `Step::Finalized`, `Step::Apply{finalized}`, `Published` gates / `Served`, `published.rs` gate
  task (durable-tip watches stay for status and metrics)
- per index: `apply_tip`, `reorg`, `ReadView`'s tier logic, `*Service` wrappers around `Served`
- `zaino-non-finalized-state` (unused since the verified-chain producer)
- zainod: per-index watchers/gates, `open_optional` tuples

## 9. Invariants

| ID  | Invariant                                                                | Where                          |
| --- | ------------------------------------------------------------------------ | ------------------------------ |
| N1  | every node's block = `VerifiedChain::hash_at` + merkle root              | `check`, fetch acceptance      |
| N2  | node layer = parent layer `.with(own Changes)`                           | `check`                        |
| N3  | a node leaves only after every enabled store's durable tip ≥ it          | `check`                        |
| N4  | `snap.tip` folded, on the verified best, ≥ root                          | `check`, before `Publish`      |
| N5  | final stream: every height once, ascending, never retracted              | `check`, writer `apply` assert |
| N6  | every index read through a snapshot = folding it from genesis along best | model                          |
| P1  | `view()` = committed prefix; `staged()` = committed + buffered           | conformance                    |

## 10. Tests

- `NfsCore` model: random `VerifiedChain` evolutions (extend, reorg at random depth, same-height
  replacement, retreat, finalize), bodies honest / wrong / poisoned / slow / missing, durable acks
  delayed, restarts; stores on an in-memory engine. Oracle: fold every index from genesis along
  best; every published snapshot answers like it at every height ≤ its tip.
- Fire drills: one planted bug per `check()` assertion and precondition.
- Per index: fold golden bytes + reader tests over `LayeredView` (in-memory engine).
- Port conformance gains `apply`/`commit`/`staged` and `Layer` steps.
- Live: S1–S16 reorg group, W2 servable tip, R12 cross-RPC agreement.

## 11. Implementation plan (parallel)

Interfaces in §4–§7 are the contract between agents.

| Wave | Agent | Scope                                                                                    | Depends on                       |
| ---- | ----- | ---------------------------------------------------------------------------------------- | -------------------------------- |
| 1    | A     | port: `Store::apply/commit/staged`, `Layer`, `LayeredView`, conformance, `DiskStore`     | —                                |
| 1    | B     | `zaino-nfs` core + graph + fetch (moved) + model + fire drills, over a toy index         | §6 types                         |
| 1    | C     | folds + readers: compact-block, value-balance, block-hash                                | `SequenceRead`/`MapRead` (exist) |
| 1    | D     | folds + readers: tree-state (`fold_run`), transparent-address                            | same                             |
| 2    | E     | writers on the final stream + `Store::apply`; delete `Tiered`, `Step` variants           | A, C, D                          |
| 2    | F     | `zaino-nfs` driver + `fold_block` with the real folds                                    | B, C, D                          |
| 3    | G     | gRPC on `Snapshot`, zainod boot + config, delete gates / `Served` / producer / NFS crate | E, F                             |
| 4    | —     | docs, changesets, heavy runs, live suite                                                 | all                              |

## 12. Decisions for review

1. **Bulk folds in the writers, `FeeSink` in bulk only** (value-balance → compact-block). Keeps
   per-index pipelining across blocks during first sync; the tip uses fold order.
1. **Lockstep finality at the tip**: one root for every index.
1. **Fold on demand**: a node is folded when it joins the verified best, not on every side branch.
1. **Rebase layers at each finality step** (bounded by `finalised_depth`), measured before tuning.
1. **The NFS owns fetching for every height** (absorbs the producer): one sender on the final
   stream, one fetch scheduler, no handoff between "bulk" and "tip" components.
1. **`view()` = committed only** for serving; `staged()` only for bulk folds.
