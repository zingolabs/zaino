# zaino-nfs: one non-finalized state, folds, one snapshot

Status: **implemented** (2026-10-07): §4–§8 describe the code. The final stream's mechanics are
in [data-sink.md](data-sink.md).

Builds on [verified-chain.md](verified-chain.md) (the header chain decides best and final) and
[persistence-engine.md](persistence-engine.md) (the port).

## 1. Why

"The chain above the final tip" used to live in four places: the header chain's best path, the
producer's held blocks, every index's RAM tier (`Tiered::apply`/`reorg`), and every index's
serving gate. A reorg was replayed by the producer and re-applied by five indexes. A gRPC request
pinned one index's view, so `GetLatestBlock`, `GetBlockRange` and `GetTreeState` agreed only
because each index was gated separately (test plan R12). All four are deleted (§8).

Now there is one of each:

| Concept                             | One place                                                 |
| ----------------------------------- | --------------------------------------------------------- |
| which chain is best, what is final  | `zaino-header-chain` (`VerifiedChain`)                    |
| index state as of a non-final block | `zaino-nfs`: one `imbl` node per block                    |
| how a block changes an index        | that index's `fold`                                       |
| what a request reads                | one `Snapshot`                                            |
| how data becomes durable            | the final stream → writer (`Committer`) → `Store::commit` |

## 2. The whole pipeline

```text
                     trusted validators, peers
                                │ headers                     │ blocks (checked: hash + merkle)
                                ▼                             ▼
  zaino-header-chain ── VerifiedChain ──▶ zaino-nfs ◀──── fetch (any source)
     (best, final)      (watch)            │
                                           │  imbl graph, one Node per block above the root
                                           │    Node { block, folds: per-index Changes,
                                           │           layers: per-index Layer }
                                           │
                     ┌─────────────────────┼──────────────────────────┐
                     ▼                     ▼                          ▼
              Snapshot (ArcSwap)     final stream: Final{block,   fetch wants
                     │                folds: Option}              (bulk + tip)
                     │                     │
                     │        ┌────────────┼─────────────┬──────────────┐
                     │        ▼            ▼             ▼              ▼
                     │   value_balance  compact_block  tree_state  transparent / block_hash
                     │    writer ──fees──▶ writer        writer        writers
                     │        │ (bulk only) │             │              │
                     │        └────── Store::apply → Store::commit (one fsync per batch) ──┐
                     │                                                                     │
                     │◀────────────── committed views (watch, acks) ◀──────────────────────┘
                     ▼
  zaino-grpc:  let snap = handle.snapshot()?;   snap.views().compact_block()   snap.views().tree_state() …
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
| **final stream** | `IndexerDataSink<Final>`: every block exactly once, in height order, final               |

## 4. Persistence port

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
- `Tiered` is deleted: its non-final half became the NFS's layers, its staging `Store::apply`
  behind `zaino_sync::Committer`.

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

## 5. Index crates

Every index crate has the same four parts. Using compact-block:

```text
zaino-index-compact-block/src/
  lib.rs       schema(network), BLOCKS, FORMAT, re-exports
  fold.rs      pub fn fold(parent, block, fees) -> Changes       (pure; golden tests beside it)
  reader.rs    pub struct CompactBlockReader<V>                    (typed reads over LayeredView<V>)
  writer.rs    pub struct CompactBlockIndexWriter<S: Store>        (final stream → apply → commit)
  serve.rs     ServeError, block_at, resident_block, RangeCursor   (reads RPCs answer with)
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
    pub fn new(view: V, network: NetworkType) -> Self;          // a route's: snap.views().compact_block()
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
- Readers come from a snapshot (`snap.views().compact_block()`, `Option`: `None` = disabled), not
  an `XReader::at(&snap)`: `zaino-nfs` depends on the index crates, never the reverse.
- Fallible folds return `Result`: value-balance `FoldError` (missing prevout, negative fee,
  overflow), compact-block `TreeSizeOutOfRange` (#549), tree-state `FoldError` (a non-canonical
  note commitment, or parent nodes that will not rebuild a frontier). A compact-block fold onto a
  non-parent panics (value-balance's parent may be any later state: insert only).
- Runs: tree-state exports `fold_run(parent, blocks) -> Result<Vec<Changes>, FoldError>` (one
  batched Merkle hashing per run, split per block); value-balance has a crate-internal `fold_run`
  (one prevout probe per run). `fold` = a run of one. Bulk sync uses runs; the NFS folds one block.
- transparent-address's fold is a lookup-free projection (spends keyed by outpoint, unspent =
  a read-time miss in `spent`); its parent reader supplies only the network.
- Writers (`CompactBlockIndexWriter`, `ValueBalanceIndexWriter`, `BlockHashIndexWriter`,
  `TreeStateIndexWriter`, `TransparentAddressIndexWriter`): `new(store, batch_bytes)`,
  `committed()`, `run(blocks[, fees])`; one loop each, no reorg, no tiers, no gate. The store sits
  behind a `zaino_sync::Committer` (commit cadence, committed-view watch):

```rust
// CompactBlockIndexWriter::run
pub async fn run(mut self, mut blocks: Subscription<Final>, mut fees: Subscription<BlockFees>) {
    while let Some(run) = self.store.next(&mut blocks).await {         // commits when due, first
        let mut paid = Vec::with_capacity(run.unfolded.len());
        for (_, block) in &run.unfolded {
            paid.push(next_fees(&mut fees, block).await);             // one per unfolded step
        }
        let applied = move |store: &mut S| {
            let mut paid = paid.into_iter();
            run.apply(store, |store, block| {                         // held skipped, folded as sent
                let fees = paid.find(|fees| fees.height == block.header().height).expect("one per step");
                fold(&CompactBlockReader::new(store.staged(), network), block, &fees).unwrap_or_else(..)
            });
        };
        self.store.compute(applied).await;                             // CPU pool, never the loop
    }
    assert!(matches!(fees.next().await, Step::Shutdown));            // fees end with the blocks
}
```

- `Committer::next` commits when the buffer reaches `batch_bytes`, after each run carrying folded
  steps (the tip), and when the stream is quiet for 1 s (lockstep: the first tip fold waits for
  every index to hold all it was sent); `Shutdown` = a last commit, then `None`.
- `committed()` = the `watch::Receiver` handed to `Nfs::subscribe`: its view's tip is the durable
  tip, and the view itself is what snapshots and root folds read.
- Tree-state and value-balance fold a run's unfolded steps with one `fold_run` and apply folded
  ones with `Run::apply_folded`; the rest use `Run::apply` per block.
- Fees in bulk: value-balance sends one per unfolded step, re-folding a height it holds (insert
  only: any later state resolves the same), compact-block pops one per unfolded step, one it
  skips included; both queues stay in step across a restart with either index ahead.

## 6. zaino-nfs

```text
zaino-nfs/src/
  lib.rs        Nfs (driver), NfsError, re-exports
  core.rs       NfsCore<F>: pure state machine (no I/O, time as input), check()
  graph.rs      Node<F>; imbl::HashMap<BlockHash, Arc<Node<F>>>, side-node pruning
  fold.rs       Folded, FoldError, fold_block: the fold order, the one place indexes meet
  fetch.rs      check_block, wants, hedging, blame
  emit.rs       metrics (zaino_best_tip, zaino_reorgs_total, zaino_fetch_*)
  report.rs     `Syncing blocks` progress every 30 s
  snapshot.rs   Snapshot<V>, Views<V>, ChainParams, NfsHandle<V>, PerIndex<T>
  core/model.rs, core/fire_drills.rs, tests.rs (driver end to end)
```

### Driver

```rust
pub struct Nfs<S, V> { /* chain watch, sources, params, sink, committed watches, root layers, publisher */ }

impl<S: ChainDataSource, V: SequenceRead + MapRead> Nfs<S, V> {
    pub fn new(chain: watch::Receiver<Option<Arc<VerifiedChain>>>, sources: Vec<Arc<S>>,
               params: ChainParams, lookahead: NonZeroUsize, depth: ReorgDepth) -> Self;
    // panics: kind twice, CompactBlock before ValueBalance
    pub fn subscribe(&mut self, kind: IndexKind, committed: watch::Receiver<V>, queue: NonZeroUsize)
        -> Subscription<zaino_sync::Final>;
    pub fn handle(&self) -> NfsHandle<V>;
    pub fn subscribe_handed(&self) -> watch::Receiver<Option<Height>>;  // last block handed over
    pub async fn run(self, cancel: CancellationToken) -> Result<(), NfsError>;
}

pub enum NfsError { Diverged { index: &'static str, height, expected, got }, Fold(FoldError), ChainGone, WriterGone(&'static str) }
```

- One task: `select!` over the chain watch, finished fetches and folds (one `JoinSet`), each
  index's next commit (one `watch::changed` per index, respawned), a 1 s tick; every output
  executed in order; `check()` after each step in debug builds.
- `Fetch` → a task (`getblock <hash> 0` + `check_block`); `Fold` → `zaino_sync::compute` (never on
  the async loop); `Send` → `Step::Apply { height, data: Final { block, folds } }`, awaited
  (backpressure); `Publish` → a `Snapshot` swapped into the handle.
- `committed` (per index) = the store's committed view after each commit: its tip = the core's
  `Durable` input, the view itself = what root folds and snapshots read. One map holds both, so a
  fold or snapshot pairs layers with exactly the durable state the core knows.
- Observability (`describe_metrics()`; names = ztest's `zainod` families):

| Signal                                                          | Source                                                                          |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `zaino_best_tip`                                                | each verified chain's best height                                               |
| `zaino_reorgs_total` + WARN `Chain reorg detected`              | a published tip that left the best chain (from, to)                             |
| `zaino_fetch_height`, `zaino_fetch_*_total`, `subscribe_handed` | each block handed to the indexes: folded, or sent unfolded (rewinds on a reorg) |
| INFO `Chain tip advanced`                                       | each published tip that is the verified best                                    |
| INFO `Syncing blocks` / WARN `Block fetch stalled`              | every 30 s while the handed height trails the best                              |

### Fold

```rust
// core: generic over the per-node payload F (the driver: Folded; the model: a toy fold)
pub(crate) struct Node<F> { at: BlockRef, parent: BlockHash, block: Arc<Block>, folded: Arc<F> }

pub(crate) struct Folded {
    folds: Arc<zaino_sync::Folds>,           // the final stream's payload: Changes per enabled index
    layers: PerIndex<Layer>,                 // per enabled index: parent's layer.with(own Changes)
}

pub(crate) fn fold_block<V: SequenceRead + MapRead>(parent: &Views<V>, block: &Block)
    -> Result<Folded, FoldError>;            // FoldError = ValueBalance | CompactBlock | TreeState
```

- `parent` = the committed views + the parent node's layers (`Output::Fold.parent = None`: the
  root, empty layers), the same `Views` a snapshot serves through.
- No `fees` in `Folded`: compact-block's `Changes` already carry them; nothing else reads them.

### Core

```rust
pub(crate) enum Input<F> {
    Chain(Arc<VerifiedChain>),
    Body { from: usize, at: BlockRef, answer: Answer },    // Checked (check_block) | Misanswered | Failed
    Folded { at: BlockRef, folded: Arc<F> },
    Durable { index: usize, tip: Option<BlockRef> },       // index = position in `new`'s durable tips
    Tick,
}

pub(crate) enum Output<F> {
    Fetch { from: usize, height: Height, record: Record },  // driver: getblock + check_block(record)
    Misanswered { from: usize, at: BlockRef, why: Misanswer },
    Unserved { height: Height },                            // every source out: retried after 1 s
    Fold { at: BlockRef, parent: Option<Arc<F>>, block: Arc<Block> },  // None = committed stores at the root
    Send(Final<F>),                                         // to the final stream, in list order
    Publish(SnapshotTip<F>),                                // driver builds the Snapshot from it
}

pub(crate) struct Final<F> { block: Arc<Block>, folded: Option<Arc<F>> }
pub(crate) struct SnapshotTip<F> { chain: Arc<VerifiedChain>, tip: BlockRef, folded: Option<Arc<F>> }

impl<F> NfsCore<F> {                                        // crate-internal: the driver is its one user
    fn new(sources: usize, lookahead: usize, depth: ReorgDepth, durable: Vec<Option<BlockRef>>) -> Self;
    fn step(&mut self, input: Input<F>, now: Instant) -> Result<Vec<Output<F>>, Diverged>;
    fn check(&self);                                        // N1–N5, named panics (N6: model + driver test)
}
```

- `Err(Diverged)` = an index's durable block off the final chain (resync); a durable tip above
  the final tip (a lost header store) holds sends, folds and publishes until the chain covers it.
- **Lockstep emerges**: a block folds on the root only above the final tip, and root ≤ sent ≤
  final, so the first tip fold waits for every index to hold everything sent. A writer therefore
  commits when its stream idles, not only at `batch` bytes.
- Between the root and the last sent block the stream is all folded or all unfolded: once a node
  exists, the next final height is folded too (never sent unfolded over a folded parent).
- Side nodes: `VerifiedChain` exposes only the best path, so the NFS applies the header chain's own
  rules (H2, H4): a side node goes once its fork is below the final tip, and past `4 · depth` side
  nodes the lowest side leaf goes.

A block's life:

```text
header verified ─▶ on best? ─▶ fetch (any source) ─▶ checked (hash_at + merkle)
   ─▶ final, parent unfolded? ── yes ─▶ Send(Final{block, folded: None})       (bulk)
                             └─ no ──▶ Fold (parent node, or the root) ─▶ node joins graph
                                  ─▶ Publish(snapshot at deepest folded best node)
   ─▶ final ─▶ Send(Final{block, folded: Some}) ─▶ every store acks ─▶ root advances,
       node pruned (later views rebase its successors' layers)
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
pub struct Snapshot<V> { chain: Arc<VerifiedChain>, tip: BlockRef, params: ChainParams, views: Views<V> }
impl<V> Snapshot<V> {
    pub fn chain(&self) -> &Arc<VerifiedChain>;
    pub fn tip(&self) -> BlockRef;                         // folded on the verified best, else the root
    pub fn params(&self) -> ChainParams;                   // { network, activations: PoolActivations }
    pub fn views(&self) -> &Views<V>;
}

/// Every enabled index as of one block: snapshot state and fold parent alike
pub struct Views<V> { network: NetworkType, durable: PerIndex<V>, layers: PerIndex<Layer> }
impl<V: SequenceRead> Views<V> {
    pub fn compact_block(&self) -> Option<CompactBlockReader<LayeredView<V>>>;   // None = disabled
    pub fn tree_state(&self) -> Option<TreeStateReader<LayeredView<V>>>;
}
impl<V: MapRead> Views<V> {
    pub fn block_hash(&self) -> Option<BlockHashReader<LayeredView<V>>>;
    pub fn transparent_address(&self) -> Option<TransparentAddressReader<LayeredView<V>>>;
    pub(crate) fn value_balance(&self) -> Option<ValueBalanceReader<LayeredView<V>>>;  // folds only
}

pub struct NfsHandle<V> { current: Arc<ArcSwapOption<Snapshot<V>>>, changed: watch::Receiver<()> }
impl<V> NfsHandle<V> {
    pub fn snapshot(&self) -> Option<Arc<Snapshot<V>>>;   // one atomic load
    pub async fn changed(&mut self) -> Result<(), RecvError>;  // next publish; Err = driver stopped
}
// feature `testing` (consumers' route tests, no driver):
impl<V: View> NfsHandle<V> {
    pub fn unpublished() -> Self;                                            // nothing served yet
    pub fn fixed(chain, tip: BlockRef, params, durable: impl IntoIterator<Item = (IndexKind, V)>) -> Self;
}
```

- Every request or stream pins one snapshot for its life (nodes through `Arc`, disk through the
  pinned view): a commit or reorg mid-stream cannot move what it reads.
- `GetLatestBlock` = `snap.tip()`; every RPC answers at heights `≤ snap.tip()`: they agree by
  construction (R12 closed, W2 "already servable" holds by definition).
- **Rebase on build** (decision 4): nodes are immutable; building a `Views` (each snapshot, each
  fold parent) rebases the node's layers onto the committed views it pairs with. Cost per build =
  the blocks committed since that node folded (≤ the root's lag), never the whole layer.
- Bulk sync: no nodes; snapshot = committed views alone at the root (the lowest durable tip). An
  index ahead of the root reads past `tip()` there (its committed view alone): routes serve at
  `tip()`. At a folded tip every view's tip = `tip()`.
- An index enabled later: the root is the lowest durable tip of every enabled index, so its tip
  holds the served tip back until it catches up (open: let it bulk-sync alone, `None` meanwhile).

## 7. gRPC and zainod

```rust
// zaino-grpc: routes hold the handle, not per-index views
pub struct Routes<S, V> {
    pub chain: Arc<ChainView<S>>, pub validators: TrafficBalancer<S>, pub network: NetworkType,
    pub nfs: NfsHandle<V>, pub max_address_rows: NonZeroUsize,
}

// per index request (Wired::answer): one snapshot, pinned for the request or stream
let snap = self.snapshot()?;                                  // None = UNAVAILABLE (booting)
let Some(blocks) = snap.views().compact_block() else { return not_enabled(..) }; // UNIMPLEMENTED
blocks::dispatch(&snap, blocks, path, body, reads).await      // RangeCursor::new(blocks, start, end, snap.tip().height, pools)
```

- Every index method answers at heights `≤ snap.tip()`: `GetLatestBlock` = the tip itself;
  `GetBlock` / `GetTreeState` past it = `NOT_FOUND`; ranges clamp to it; a by-hash locate past it =
  `NOT_FOUND`; `GetSubtreeRoots` = roots completing at or below it; the transparent reader is
  `.as_of(snap.tip().height).with_max_rows(max_address_rows)`.
- Tree-state memos (layer heights, the tip, each pool's roots) are keyed per snapshot.
- `GetLightdInfo.blockHeight` = the snapshot tip (0 before the first). Compact-block is optional
  like every index.

```rust
// zainod indexer::pipeline (boot = chain view + this + chain-view tasks + supervise)
let nfs = Nfs::new(inputs.chain, inputs.sync, params, config.sync.concurrency, depth);
let mut indexes = Subscribed { nfs, opened: Vec::new() };
let mut tasks = JoinSet::new();                          // after the NFS: dropped first on an early Err

if let Some((cb, vb)) = config.compact_block()? {
    // compact-block folds after value-balance (its fees)
    let mut fee_sink = FeeSink::new("fees");
    let fees = fee_sink.subscribe(IndexKind::CompactBlock.name(), cb.queue_bytes);
    let (span, writer) = open(&engine, &vb, value_balance::schema(network), ValueBalanceIndexWriter::new)?;
    let blocks = indexes.subscribe(IndexKind::ValueBalance, writer.committed(), &vb, &span);
    spawn_index(&mut tasks, IndexKind::ValueBalance, span, writer.run(blocks, fee_sink));
    let (span, writer) = open(&engine, &cb, compact_block::schema(network), CompactBlockIndexWriter::new)?;
    let blocks = indexes.subscribe(IndexKind::CompactBlock, writer.committed(), &cb, &span);
    spawn_index(&mut tasks, IndexKind::CompactBlock, span, writer.run(blocks, fees));
}
// block_hash, tree_state, transparent_address: same three lines, no fees
let snapshots = indexes.nfs.handle();
let server = GrpcService::new(Routes { nfs: snapshots.clone(), .. }, address, limits).bind().await?;
let Subscribed { nfs, opened } = indexes;                // nothing fallible past here
spawn(&mut tasks, "nfs", component("ZainoNFS"), nfs.run(cancel.child_token()));
spawn(&mut tasks, "grpc", grpc_span, server.run(cancel.child_token()));
spawn(&mut tasks, "serving", .., serving::run(snapshots, verified, depth, synced, ..));
// per opened index: metrics, index report, /statusz source
```

- `serving::run` = `zaino_index_synced` + `/readyz`: on once the served tip **is** the verified
  best, off once it leaves the best chain or trails it by more than `depth`.

## 8. Deleted

- `zaino_persistence::Tiered` and its tests (→ `Store::apply`/`staged` + `Layer`/`LayeredView` +
  `zaino_sync::Committer`)
- `zaino-sync`: the producer (`Producer`, `ProducerCore`, `ProduceError`, its model and fire
  drills, `tests/reorg_model.rs`); `Step::{Finalized, Reorg}` and `Step::Apply.finalized`;
  `Published` (serving gate, reorg counter, `merged`); `Served` / `Reads`; `BlockSink`;
  `Offloaded` / `blocking` made crate-private
- `CompactBlockService`, `TreeStateService`, `TransparentAddressService`, `BlockHashService` (+
  block-hash `serve.rs`): their logic moved onto the readers and the routes
- the `zaino-non-finalized-state` crate
- `zaino-source`: `BlockFetchPool`, `TrafficBalancer::among`, `ChainDataSource::get_block`
- zainod: per-index `Watchers`, `open_optional` tuples, serving-gate tasks, the compact-block
  "cannot be disabled" refusal

## 9. Invariants

| ID  | Invariant                                                                                              | Where                                                                    |
| --- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------ |
| N1  | every node's block = `VerifiedChain::hash_at` + merkle root                                            | `check`, fetch acceptance                                                |
| N2  | node above the root, folded on a held parent (node or root); layer = parent layer `.with(own Changes)` | `check`; `fold_block`, `LayeredView::new` + `rebase` panics, driver test |
| N3  | a node leaves only after every enabled store's durable tip ≥ it                                        | `check`, `Durable` asserts, model                                        |
| N4  | `snap.tip` = deepest folded best block (else the root)                                                 | `check`, model, driver test                                              |
| N5  | final stream: every height once, ascending, never retracted                                            | `check`, model + driver writers                                          |
| N6  | every index read through a snapshot = folding it from genesis along best                               | model, driver test (real folds)                                          |
| P1  | `view()` = committed prefix; `staged()` = committed + buffered                                         | conformance                                                              |

## 10. Tests

- `NfsCore` model: random `VerifiedChain` evolutions (extend, reorg at random depth, same-height
  replacement, retreat, finalize), bodies honest / wrong / poisoned / slow / missing, durable acks
  delayed, restarts; stores on an in-memory engine. Oracle: fold every index from genesis along
  best; every published snapshot answers like it at every height ≤ its tip.
- Fire drills: one planted bug per `check()` assertion and precondition.
- Driver (`zaino-nfs/src/tests.rs`): all five real folds over `SimFs` stores, mock validators, a
  test-only committer per index; bulk, reorgs (longer, same height, retreat), finality, a crash
  restart with indexes apart. Every snapshot seen = each index folded from genesis along best into
  fresh stores (table by table); every view at the snapshot tip (R12); final stream per index =
  each final height once. `fold_block` golden in `fold.rs`.
- Per index: fold golden bytes + reader tests over `LayeredView` (in-memory engine); each writer:
  folded vs unfolded steps, held heights skipped on restart, crash states, a random-stream model.
- `zaino-sync`: the `Committer` contract (batch, folded-run and idle commits; restart skip; gap and
  unfolded-after-folded panics).
- `zaino-grpc`: R12 on one fixed snapshot (`GetLatestBlock`, `GetBlockRange`, `GetTreeState` at its
  tip, views ahead of it unseen). zainod: the whole pipeline over a mock validator, through a reorg.
- Port conformance gains `apply`/`commit`/`staged` and `Layer` steps.
- Live: S1–S16 reorg group, W2 servable tip, R12 cross-RPC agreement.

## 11. Implementation plan (done)

| Wave | Scope                                                                                 | Status                   |
| ---- | ------------------------------------------------------------------------------------- | ------------------------ |
| 1    | port: `Store::apply/commit/staged`, `Layer`, `LayeredView`, conformance, `DiskStore`  | done                     |
| 1    | `zaino-nfs` core + graph + fetch + model + fire drills                                | done                     |
| 1    | folds + readers: every index                                                          | done                     |
| 2    | `zaino-nfs` driver + `fold_block` with the real folds                                 | done                     |
| 3    | writers on the final stream, gRPC on `Snapshot`, zainod boot + config, deletions (§8) | done                     |
| 4    | docs, changesets, heavy runs                                                          | done; live suite pending |

## 12. Decisions

1. **Bulk folds in the writers, `FeeSink` in bulk only** (value-balance → compact-block). Keeps
   per-index pipelining across blocks during first sync; the tip uses fold order.
1. **Lockstep finality at the tip**: one root for every index.
1. **Fold on demand**: a node is folded when it joins the verified best, not on every side branch.
1. **Rebase layers as views are built** (each snapshot, each fold parent; nodes stay immutable),
   bounded by the blocks committed since the node folded; measured before tuning.
1. **The NFS owns fetching for every height** (absorbs the producer): one sender on the final
   stream, one fetch scheduler, no handoff between "bulk" and "tip" components.
1. **`view()` = committed only** for serving; `staged()` only for bulk folds.
