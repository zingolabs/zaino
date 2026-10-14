# `zaino-sync` — usage

The final path: `FinalizedFollower` fetches every final block in order onto one byte-bounded
broadcast (`IndexerDataSink<Block>`); each index writer runs its own loop over its store
(`apply` / `commit`) and publishes it through an `IndexPublisher`. Where it fits:
[architecture §7](../../docs/architecture.md#final-path-zaino-sync).

```text
 VerifiedChain ─▶ FinalizedFollower ─▶ IndexerDataSink<Block> ─┬─▶ [queue] ─▶ utxo-set ─▶ FeeSink ┐
 (final_tip)      fetch, in order  one Arc per step        ├─▶ [queue] ─▶ compact-block ◀────────┘
                                                           ├─▶ [queue] ─▶ tree-state
                                                           ├─▶ [queue] ─▶ block-hash
                                                           └─▶ [queue] ─▶ transparent-address
                                                       each writer: fold → Store::apply → commit
```

Which index holds the pipeline back: the one whose `zaino_index_applied_height` stops while
`zaino_fetch_height` waits. Why: its `zaino_index_run_seconds` against `zaino_index_write_seconds`
(run ≈ write = the disk; run ≫ write = the fold, or compact-block waiting on utxo-set's fees).

```rust,ignore
use zaino_sync::{apply, blocking, commit, held, IndexHandle, IndexPublisher, Subscription};

pub struct MyIndexWriter<S: Store> { store: S, publisher: IndexPublisher<S::View> }

impl<S: Store<View: MapRead>> MyIndexWriter<S> {
    pub fn new(store: S) -> Self {                      // store opened with its WRITE_BUFFER
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    pub fn handle(&self) -> IndexHandle<S::View> {      // → FinalizedFollower::subscribe + Nfs::add
        self.publisher.handle()
    }

    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            store = blocking(move || {
                for (height, block) in &run.blocks {
                    if !held(&store, *height) {
                        let mut changes = store.changes(block.at());
                        fold(&MyReader::new(store.applied()), block, &mut changes);
                        apply(&mut store, changes);     // full buffer = the store commits
                    }
                }
                if run.finalized_tip {
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

## `FinalizedFollower`

- `new(chain, balancer, lookahead)`; `subscribe(kind, handle.tip(), queue)` per index;
  `progress()`; `run(cancel)`.
- Streams `lowest durable + 1 ..= final tip`, `lookahead` blocks in flight, delivered in order;
  waits for the next chain past the final tip.
- A height the verified chain holds a header for (the final window and above) is fetched by
  hash from any member, peers included (`fetch`), checked against that header. Below it (bulk
  sync) it goes by height to trusted members only (`fetch_at` → `TrafficBalancer::blocks_at`,
  as many per request as one reply at `ResponseSizeCap::MAX` holds): no header there to hold a
  peer to. A reply short of its heights is a misanswer (`Misanswer::Count`), asked again.
- Each block's parent must be each durable tip it extends (`FollowError::Diverged`, naming the
  index: resync) and the block sent before it (`FollowError::Unlinked`: the validator's history
  moved). Either stops the follower; nothing is skipped.
- Cancel → `Ok`; either way every queue ends with `Shutdown`.
- By height (internal): one batch from a trusted member
  until every block's coinbase height matches its place in the batch (a misanswer `report`ed,
  the batch asked again). The body is the trusted member's word, not re-hashed; regressions in
  decoding are the job of the live sync test (`live-tests/sync`), which holds blocks, tree states
  and a wallet's scan to zebra.
- `fetch(balancer, at, record, urgency)` / `check_block`: by hash, held to a verified header
  (hash, coinbase height, `check_body`); the NFS's and the follower's.
- `Misanswer::{WrongBlock, WrongHeight, Body(BodyMismatch)}` names the check that failed.
- `SyncProgress`: `handed()` = last height sent, `blocks()` = blocks sent since boot (atomics).

## `IndexHandle`

What the rest of the daemon knows about one index: `view()` (committed), `tip()` (durable),
`applied()` (last block folded into the store, committed or not), `changed()` (after each
commit; `false` = writer gone).

## `IndexerDataSink<T>`

- `new()`, then `subscribe(name, budget)` per subscriber (`name` = its panic messages); every
  subscriber joins first.
- `send(step)`: the same step to every queue (one `Arc<T>`), all or nothing; a full queue waits
  (the slowest subscriber paces the publisher).
- `shutdown()`: `Step::Shutdown` last in every queue, never waits.
- `Subscription::next()`; `next_run()` → `Some(Run)` = next step + every `Apply` already
  queued, to the queue's budget (one batch of work, no wait past the first step) | `None` at
  `Shutdown`.
- `Run { blocks, finalized }`: `finalized` = its last block = `Step::FinalizedTip` (the chain's final
  tip; ends the run).

## Writer loop

- `held(store, height)`: at or below the staged tip (a restart resends from the lowest durable tip).
- `apply` / `commit` return the time spent in the store; `ran(&store, started, write)` after each
  run records `zaino_index_run_seconds` (taken off the queue → published, waits included) and
  `zaino_index_write_seconds` (that run's applies + commits), both `{index}`: run − write = the
  fold + waits. Buckets: `METRIC_BUCKETS`.
- `assert_next(out, parent_tip, block)` / `assert_run(parent_tip, blocks, outs)`: a fold's
  preconditions, panicking naming the index: `out` opened for `block` (`out.block()`), `block` one
  height above `parent_tip` linked by `prev_hash` (genesis on an empty parent).
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

`FeeSink` = `IndexerDataSink<BlockFees>`: utxo-set publishes one per step (held heights read
back from its stored fees), compact-block pops one per step, so the two stay in step with either
one ahead.

## Failure: panic, never `Err`

| Failure | Panic message |
|---|---|
| commit hits a full disk | `<index> index commit failed: disk <dir> full` |
| any other commit error | `<index> index commit failed at <dir>: <error>` |
| chain data the index cannot take | `<index> index: <error>` |
| a broken stream (a gap, out-of-step fees) | the `assert!` message, naming the index |

## Testing a writer

`zaino_sync::testing` (feature `testing`, a dev-dependency): the shared kit every index writer's
tests use; each crate keeps only its own fold oracle.

```rust,ignore
use zaino_sync::testing::{block_on_paused, finalized, moves, open, step, write_buffers,
    FinalStream, Move, Writer, QUEUE};

fn start(store: DiskStore) -> Writer<DiskView> {
    let writer = MyIndexWriter::new(store);
    Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
}

let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));  // SimFs, at /<index name>
writer.send(step(&blocks[0])).await;
writer.send(finalized(&blocks[1])).await;                     // commits after it
writer.durable_at(h(1)).await;                                // None = nothing committed
let handle = writer.stop().await.expect("stops at Shutdown"); // Err = the writer panicked
```

- `open(fs, schema, write_buffer)`: `write_buffer` = MIN commits every applied block.
- `Writer::stop` → `Err(JoinError)` on a writer panic: `#[should_panic]` tests re-raise it with
  `std::panic::resume_unwind(error.into_panic())`.
- Random histories: `moves()` (1..16 of `Move::Send(1..=3)` / `Move::Reopen`, 4 : 1) and
  `write_buffers()` (MIN | MAX) as proptest strategies; `FinalStream::new(&blocks).steps(next)`
  = the steps a move sends (a `Send`'s last `FinalizedTip`, a `Reopen`'s = the last sent block again,
  into the restarted writer), `tip()` = the durable tip to wait for; `block_on_paused(history)`
  runs the async body on a current-thread runtime, clock paused.
- Fold preconditions (`assert_next` / `assert_run`) and the commit-failure panic are tested here,
  once; an index's tests cover only its own fold.
