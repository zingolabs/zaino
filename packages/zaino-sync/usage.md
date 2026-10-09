# `zaino-sync` — usage

The final path: `FinalFollower` fetches every final block in order onto one byte-bounded
broadcast (`IndexerDataSink<Block>`); each index writer runs its own loop over its store
(`apply` / `commit`) and publishes it through an `IndexPublisher`. Design:
[the final path](../../docs/design/data-sink.md); where it fits:
[pipeline.md](../../docs/design/pipeline.md).

```rust,ignore
use zaino_sync::{apply, blocking, commit, held, IndexHandle, IndexPublisher, Subscription};

pub struct MyIndexWriter<S: Store> { store: S, publisher: IndexPublisher<S::View> }

impl<S: Store<View: MapRead>> MyIndexWriter<S> {
    pub fn new(store: S) -> Self {                      // store opened with its WRITE_BUFFER
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    pub fn handle(&self) -> IndexHandle<S::View> {      // → FinalFollower::subscribe + Nfs::add
        self.publisher.handle()
    }

    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            store = blocking(move || {
                for (height, block) in &run.blocks {
                    if !held(&store, *height) {
                        let mut changes = store.changes(block.at());
                        fold(&MyReader::new(store.staged()), block, &mut changes);
                        apply(&mut store, changes);     // full buffer = the store commits
                    }
                }
                if run.finalized {
                    commit(&mut store);                 // caught up: durable now
                }
                store
            })
            .await;
            publisher.publish(&store);
        }
        store = blocking(move || { commit(&mut store); store }).await;   // Shutdown
        publisher.publish(&store);
    }
}
```

## `FinalFollower`

- `new(chain, balancer, lookahead)`; `subscribe(kind, handle.tip(), queue)` per index;
  `progress()`; `run(cancel)`.
- Streams `lowest durable + 1 ..= final tip`, by height from trusted members
  (`fetch_at` → `TrafficBalancer::blocks_at`, 4 per batched `getblock` request), `lookahead`
  blocks in flight, delivered in order; waits
  for the next chain past the final tip. Reads only `final_tip().height`: no header history.
- Each block's parent must be each durable tip it extends (`FollowError::Diverged`, naming the
  index: resync) and the block sent before it (`FollowError::Unlinked`: the validator's history
  moved). Either stops the follower; nothing is skipped.
- Cancel → `Ok`; either way every queue ends with `Shutdown`.
- `fetch_at(balancer, heights, urgency)` / `check_block_at`: one batch until every body's
  coinbase height and merkle root match its own header (a misanswer `report`ed, the batch asked
  again).
- `fetch(balancer, at, record, urgency)` / `check_block`: by hash, held to a verified header
  (hash, coinbase height, merkle root); the NFS's.
- `SyncProgress`: `handed()` = last height sent, `blocks()` = blocks sent since boot (atomics).

## `IndexHandle`

What the rest of the daemon knows about one index: `view()` (committed), `tip()` (durable),
`applied()` (last block folded into the store, committed or not), `changed()` (after each
commit; `false` = writer gone).

## `IndexerDataSink<T>`

- `new(name)`, then `subscribe(name, budget)` per subscriber; every subscriber joins first.
- `send(step)`: the same step to every queue (one `Arc<T>`), all or nothing; a full queue waits
  (the slowest subscriber paces the publisher).
- `shutdown()`: `Step::Shutdown` last in every queue, never waits.
- `Subscription::next()`; `next_run()` → `Some(Run)` = next step + every `Apply` already
  queued, to the queue's budget (one batch of work, no wait past the first step) | `None` at
  `Shutdown`.
- `Run { blocks, finalized }`: `finalized` = its last block = `Step::Finalized` (the chain's final
  tip; ends the run).
- `zaino_sink_queue_bytes{sink, subscriber}`: bytes each queue holds; at its budget = that
  subscriber holds the publisher back.

## Writer loop

- `held(store, height)`: at or below the staged tip (a restart resends from the lowest durable tip).
- `apply(store, changes)` = `Store::apply` + counted (`zaino_index_applied_blocks_total`, its
  records + rows into `zaino_index_applied_rows_total`). Commits: the store's own once its buffer
  reaches the `write_buffer` it was opened with (`zaino_persistence` usage).
- `commit(store)`: everything buffered → disk now; the writer's after a `finalized` run and at
  `Shutdown`.
- `IndexPublisher::new(&store)`; `handle()`; `publish(&store)` after every hop: applied tip
  (`IndexHandle::applied`, `zaino_index_applied_height`), committed view once its tip moved
  (`changed`, `zaino_index_finalized_height`).
- `blocking(f)`: the fold + apply + commit hop (store moved in and back); `compute(f)` = CPU work
  on the rayon pool.

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
durability. Send a `Finalized` step to make the writer commit without filling its buffer.
