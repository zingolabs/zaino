# `zaino-sync` — usage

The final path: `FinalFollower` fetches every final block in order onto one byte-bounded
broadcast (`IndexerDataSink<Block>`); each index writer folds and commits through a `Committer`
and exposes an `IndexHandle`. Design: [the final path](../../docs/design/data-sink.md); where it
fits: [pipeline.md](../../docs/design/pipeline.md).

```rust,ignore
use zaino_sync::{Committer, IndexHandle, Subscription};

pub struct MyIndexWriter<S: Store> { store: Committer<S> }

impl<S: Store<View: MapRead>> MyIndexWriter<S> {
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        Self { store: Committer::new(store, batch_bytes) }
    }

    pub fn handle(&self) -> IndexHandle<S::View> {      // → FinalFollower::subscribe + Nfs::add
        self.store.handle()
    }

    pub async fn run(mut self, mut blocks: Subscription<Block>) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let applied = move |store: &mut S| {
                // `out` = store.changes(block), opened by `apply` for each block not held
                run.apply(store, |store, block, out| fold(&MyReader::new(store.staged()), block, out));
            };
            self.store.compute(applied).await;      // folds + applies on the CPU pool
        }
    }
}
```

## `FinalFollower`

- `new(chain, balancer, lookahead)`; `subscribe(kind, handle.tip(), queue)` per index;
  `progress()`; `run(cancel)`.
- Streams `lowest durable + 1 ..= final tip`, `lookahead` fetches in flight, delivered in order;
  waits for the next chain past the final tip.
- Nothing sent until every durable tip at or below the final tip is the final chain's block there
  (`FollowError::Diverged`: resync). A durable tip above the final tip waits.
- Cancel → `Ok`; either way every queue ends with `Shutdown`.
- `fetch(balancer, at, record, urgency)` / `check_block`: one body until its hash, coinbase
  height and merkle root match the verified header (a misanswer `report`ed); shared with the NFS.
- `SyncProgress`: `handed()` = last height sent, `blocks()` = blocks sent since boot (atomics).

## `IndexHandle`

What the rest of the daemon knows about one index: `view()` (committed), `tip()` (durable),
`changed()` (after each commit; `false` = writer gone), `serving(best, window)` (durable within
`window` of best, and every index it `requiring`s too).

## `IndexerDataSink<T>`

- `new(name)`, then `subscribe(name, budget)` per subscriber; every subscriber joins first.
- `send(step)`: the same step to every queue (one `Arc<T>`), all or nothing; a full queue waits
  (the slowest subscriber paces the publisher).
- `shutdown()`: `Step::Shutdown` last in every queue, never waits.
- `Subscription::next()`; `run(first, budget)` = `first` + every `Apply` already queued, to
  `budget` bytes (one batch of work).
- `zaino_sink_queue_bytes{sink, subscriber}`: bytes each queue holds; at its budget = that
  subscriber holds the publisher back.

## `Committer`

- `new(store, batch)`; `handle()`; `compute(f)` (`f(&mut store)` on the CPU pool).
- `next(&mut blocks)` → `Some(Run)` | `None` at `Shutdown` (everything committed). Commits first
  when the buffer reaches `batch` or the stream stays quiet for 1 s. Panics on a gap above the
  staged tip.
- `Run { blocks }`: `apply(store, fold)` folds each block `store` lacks; `apply_batch(store, fold)
  -> T` folds them as one batch (tree-state's hashing, value-balance's prevout probe).
- `held(store, height)`: at or below the staged tip (a restart resends from the lowest durable tip).

## Fees: `FeeSink`

`FeeSink` = `IndexerDataSink<BlockFees>`: value-balance publishes one per step (held heights
re-folded), compact-block pops one per step, so the two stay in step with either one ahead.

## Failure: panic, never `Err`

| Failure | Panic message |
|---|---|
| commit hits a full disk | `<index> index commit failed: disk <dir> full` |
| any other commit error | `<index> index commit failed at <dir>: <error>` |
| chain data the index cannot take | `<index> index: <error>` |
| a broken stream (a gap, out-of-step fees) | the `assert!` message, naming the index |

## Testing a writer

Send `Step`s into a real `IndexerDataSink<Block>` (and `FeeSink`), spawn `run`, wait on
`handle().changed()`, read through a reader over the committed view, reopen the store for
durability. With `#[tokio::test(start_paused = true)]` the 1 s idle commit costs nothing.
