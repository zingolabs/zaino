# `zaino-sync` — usage

The plumbing between the non-finalized state and the index writers: the final stream
(`IndexerDataSink<Final>`), its byte-bounded queues, fees from one index to another (`FeeSink`),
and the `Committer` every writer commits through. The design, with the commit cadence and fees, is
[the data sink](../../docs/design/data-sink.md); the sender is
[`zaino-nfs`](../zaino-nfs/usage.md) ([nfs.md](../../docs/design/nfs.md)).

```rust,ignore
use zaino_sync::{Committer, Final, Subscription};

pub struct MyIndexWriter<S: Store> { store: Committer<S> }

impl<S: Store<View: MapRead>> MyIndexWriter<S> {
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        Self { store: Committer::new(store, batch_bytes) }
    }

    pub fn committed(&self) -> watch::Receiver<S::View> {   // → Nfs::subscribe
        self.store.committed()
    }

    pub async fn run(mut self, mut blocks: Subscription<Final>) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let applied = move |store: &mut S| {
                let network = store.schema().network;
                run.apply(store, |store, block| fold(&MyReader::new(store.staged(), network), block));
            };
            self.store.compute(applied).await;      // folds + applies on the CPU pool
        }
    }
}
```

## `IndexerDataSink<T>`

A broadcast queue that decides nothing: one publisher, the same steps to every subscriber.

- `IndexerDataSink::new(name)`, then `subscribe(name, budget)` per subscriber → a
  `Subscription<T>`; every subscriber joins before the publisher runs
- `send(step)` → the same step to every queue (one `Arc<T>` shared), all or nothing: every queue's
  share of the budget is held before any push, so a send cancelled mid-wait delivers nowhere
- `shutdown()` consumes the sink: `Step::Shutdown` last in every queue, past a full budget (never
  waits); a subscription pops `Shutdown` again on every later call
- `Step` = `Apply { height, data }` | `Shutdown`
- `Subscription::next()`; `Subscription::run(first, budget)` = `first` + every `Apply` already
  queued behind it to `budget` bytes, never a wait (one batch of work)

### Backpressure

`T: Weight` names what an item holds in memory (`Final` = block + its `Changes`, `BlockFees`,
`Block`). Each queued step holds that many bytes of its subscriber's budget until popped; a step
heavier than the whole budget passes alone once the queue drains. A slow subscriber holds the
publisher back instead of growing memory.

`zaino_sink_queue_bytes{sink, subscriber}` (`describe_metrics()`) = the bytes each queue holds:
+ on send, − on pop. At its budget = that subscriber is holding back the publisher. The NFS's own
metrics (`zaino_best_tip`, `zaino_reorgs_total`, `zaino_fetch_*`) live in `zaino-nfs`.

## The final stream: `Final`

`Final { block: Arc<Block>, folds: Option<Arc<Folds>> }`: one final block, every height once,
ascending. `folds = None` (at or below the NFS root, bulk sync) = the writer folds it; `Some` =
the NFS folded it, `folds.get(kind)` = this index's `Changes`. Never unfolded after folded until a
restart.

## `Committer`

One writer's store + the committed-view `watch` the NFS reads.

- `Committer::new(store, batch)`: `batch` = buffered bytes per bulk commit, and one run's stream
  bytes
- `committed()`: the receiver for `Nfs::subscribe` (sent after every commit; tip = durable tip)
- `next(&mut blocks)` → `Some(Run)` | `None` at `Shutdown` (everything committed). Commits first
  when the buffer reaches `batch`, after a run carrying folded steps, or when the stream stays
  quiet for 1 s (lockstep: the NFS folds its first tip block once every index holds all it was
  sent). Panics on a gap above the staged tip or an unfolded step after a folded one
- `compute(f)`: `f(&mut store)` on the CPU pool (the store hops there and back)
- a failed commit panics naming the index and its directory (`StoreError::commit_failed`)

`Run { unfolded: Vec<(Height, Arc<Block>)>, folded: Vec<(Height, Arc<Folds>)> }`:

- `run.apply(store, fold)`: each step `store` does not hold, in order; unfolded ones through
  `fold(store, block)` (parent = `store.staged()`), folded ones as sent
- `run.apply_folded(store)`: the folded steps only (a writer folding its unfolded steps as one run:
  tree-state, value-balance)
- `held(store, height)`: at or below the staged tip (a restart resends from the lowest durable tip)

## Fees: `FeeSink`

`FeeSink` = `IndexerDataSink<BlockFees>`, published by
[`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md) (one per unfolded
step, held heights re-folded) and read by compact-block (one per unfolded step, skipped ones
included). Folded steps send nothing: the NFS folded compact-block with the fees.

## Off the runtime

`compute(f)` runs `f` on the rayon pool (one thread per core) and awaits it; a panic resumes on the
awaiting task, the caller's tracing span rides along. Folds, encoding and applies go there, never
on the async loop; commits go to tokio's blocking pool inside the `Committer`.

`Human(Duration)` renders durations the way log lines show them (`812ms`, `9m57s`, `3d04h`).

## Failure: panic, never `Err`

| Failure | Panic message |
|---|---|
| commit hits a full disk | `<index> index commit failed: disk <dir> full` |
| any other commit error | `<index> index commit failed at <dir>: <error>` |
| chain data the index cannot take | `<index> index: <error>` |
| a broken stream (a gap, unfolded after folded, out-of-step fees) | the `assert!` message, naming the index |

A subscriber that drops its queue before `Shutdown` panics the sink's next `send`; a sink dropped
without `shutdown()` panics its subscribers' next pop. In zainod the panic hook aborts the process
and every index reopens at its last durable manifest.

## Testing a writer

Send `Step`s into a real `IndexerDataSink<Final>` (and `FeeSink`), spawn `run`, wait on
`committed()` (`wait_for` a tip), read through a reader over the committed view, reopen the store
for durability. With `#[tokio::test(start_paused = true)]` the 1 s idle commit costs nothing.
