# The final path: follower → sink → writers

Status: **implemented** (2026-10-08). See [pipeline.md](pipeline.md) for where this path fits.

Every final block reaches every index writer exactly once, in height order, and is never retracted.
Nothing here knows about reorgs or the tip: the NFS ([nfs.md](nfs.md)) serves those from RAM.

```text
 VerifiedChain ─▶ FinalFollower ─▶ IndexerDataSink<Block> ─┬─▶ [queue] ─▶ value-balance ─▶ FeeSink ┐
 (final_tip)      fetch, in order  one Arc per step        ├─▶ [queue] ─▶ compact-block ◀────────┘
                                                           ├─▶ [queue] ─▶ tree-state
                                                           ├─▶ [queue] ─▶ block-hash
                                                           └─▶ [queue] ─▶ transparent-address
                                                       each writer: fold → Store::apply → commit
```

## FinalFollower (`zaino-sync`)

```rust
impl<S: ChainDataSource> FinalFollower<S> {
    pub fn new(chain: watch::Receiver<Option<Arc<VerifiedChain>>>, balancer: TrafficBalancer<S>,
               lookahead: NonZeroUsize) -> Self;
    pub fn subscribe(&mut self, kind: IndexKind, durable: Option<BlockRef>, queue: NonZeroUsize)
        -> Subscription<Block>;
    pub fn progress(&self) -> SyncProgress;             // last height sent (zaino_fetch_height)
    pub async fn run(self, cancel: CancellationToken) -> Result<(), FollowError>;
}
```

```text
start = lowest durable tip + 1
loop:
  for h in start ..= chain.final_tip().height:   lookahead blocks in flight, delivered in order
      block = fetch_at(balancer, [h..h+4], Bulk) a trusted member's blocks, 4 per batched getblock
                                                 request; refetched until each one's coinbase
                                                 height + merkle root = its own header's
      parent = each durable tip it extends?      else Diverged (that index: resync)
      parent = the block sent before it?         else Unlinked (validator history moved: stop)
      sink.send(Apply { h, block }).await        a full queue waits here (backpressure);
                                                 h = the final tip → Finalized { h, block }
  wait for chain.changed()
Shutdown on cancel
```

- One loop, one fetch window, no graph. It replaces the bulk half of the old NFS.
- Trusted = trusted: no header history read, no proof-of-work check below the final tip. The
  zebrad serving the block already validated it; the follower only checks the body is the one
  its header commits to and that it links to what was sent before.
- `getblock "<height>" 0` from trusted members only, 4 calls per JSON-RPC batch
  (`TrafficBalancer::blocks_at`; peers answer by hash alone). A wrong body gets the batch
  `report`ed and asked again; `zaino-traffic` picks who answers.
- One stream for every index: an index far behind (enabled late) paces the others until it
  catches up.

## IndexerDataSink

| Step                          | Writer                                       |
| ----------------------------- | -------------------------------------------- |
| `Apply { height, block }`     | fold it, unless already held                 |
| `Finalized { height, block }` | fold it, then commit (the chain's final tip) |
| `Shutdown`                    | commit what is buffered, stop                |

- One `Arc<Block>` per step, shared by every queue, freed at the last pop.
- Each queue is byte-bounded (`Weight`). `send` reserves room in every queue before pushing to
  any of them, so the slowest writer paces the follower and memory stays bounded.
- An index ahead of the start pops the heights it holds and skips them (`zaino_sync::held`), so
  every queue drains in order.
- `zaino_sink_queue_bytes{sink, subscriber}`: a queue at its budget is the index holding the
  pipeline back.
- Failures are loud. A dropped subscriber panics the sink, and a dropped sink panics its
  subscribers. There is no error channel.

## Writers

```rust
// one per index crate (CompactBlockIndexWriter, TreeStateIndexWriter, …)
impl<S: Store> XIndexWriter<S> {
    pub fn new(store: S) -> Self;                              // store opened with WRITE_BUFFER
    pub fn handle(&self) -> IndexHandle<S::View>;              // cheap clone, given to the NFS
    pub async fn run(self, blocks: Subscription<Block>);       // compact-block: + its fees
}

// zaino-sync: what the rest of the daemon knows about one index
impl<V: View> IndexHandle<V> {
    pub fn view(&self) -> V;                               // committed view
    pub fn tip(&self) -> Option<BlockRef>;                 // durable tip
    pub fn applied(&self) -> Option<BlockRef>;             // last applied, committed or not
    pub async fn changed(&mut self) -> bool;               // after each commit; false = writer gone
}
```

```text
while let Some(run) = blocks.next_run().await    a step + every Apply already queued, to the
                                                 queue's budget; None = Shutdown
    store = blocking(move || {                   one hop per run, store moved in and back
        each block not held(&store, h):          changes → fold onto staged() → apply(&mut store)
        run.finalized → commit(&mut store)       caught up: durable now
    })
    publisher.publish(&store)                    applied tip; committed view once its tip moved
Shutdown: commit, publish
```

- The writer owns `store: S` + `publisher: IndexPublisher<S::View>` (its end of the
  `IndexHandle`). Tree-state and value-balance fold a run as one batch (`fold_run`).
- **Commit**: the store's own in `apply` once the buffer heap reaches the `write_buffer` it was
  opened with (the index's `WRITE_BUFFER`: bulk), the writer's `commit` after a run ending in a
  `Finalized` block and at `Shutdown`. Each publish after a commit sends the new committed view,
  which is what the NFS and snapshots read.
- `zaino_sync::{apply, commit}` = `Store::apply` counted (`zaino_index_applied_*_total`) and
  `Store::commit` panicking on failure (index + directory named).

## Fees

Compact-block's records need each transaction's fee, which only value-balance can work out.
Value-balance sends one `BlockFees` per step into a `FeeSink`, held heights included, so the two
queues stay in step whichever index restarts ahead. Compact-block pops one fee step per block step.

```text
sink ─┬─▶ value-balance ─▶ FeeSink ─┐
      └─────────────────────────────┴─▶ compact-block
```

## Deleted by this design

- `Final { block, folds }` and `Folds` in the stream. The payload is a plain `Block`.
- `Run::stretches`, `apply_sent`, `Run.tip`, and the "commit after a folded run" trigger.
- The NFS as the stream's sender, its `deliver()` future, and `Input::Delivered`.
