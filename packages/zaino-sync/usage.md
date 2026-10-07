# `zaino-sync` — usage

Zaino's index sync pipeline: `Producer` → `BlockSink` → one loop per index,
each over its own subscription. New here? Read
[the data sink](../../docs/design/data-sink.md) first: it explains the steps
this whole crate is built on, with a worked reorg. Then
[`docs/design/sync.md`](../../docs/design/sync.md) for the pipeline.

- **Produce**: `Producer` (the one task that owns the `BlockSink`) follows the
  header chain's `VerifiedChain`, fetching every block by hash from any source
  and checking it against its verified header
- **Stream**: `IndexerDataSink<T>` (one producer, N subscribers, keyed by block
  height); `BlockSink` = `IndexerDataSink<Block>`, the one stream every index
  subscribes to
- **Index**: each index spawns its own loop over its `Subscription` and
  publishes through a `Published`; nothing here drives it

No scheduler, no dependency graph, no storage, no trait to implement. Each
index owns its files, its cumulative state, and its loop.

```rust,ignore
use zaino_sync::{BlockSink, Producer};

let mut block_sink = BlockSink::new("blocks");
let blocks = block_sink.subscribe(MyIndex::NAME, queue_bytes);
let index = MyIndex::new(store, batch_bytes);
let durable = [index.durable_tip()]; // every subscriber's durable tip (height + hash)
let service = MyService::new(index.published().served());
let verified = header_sync.subscribe(); // watch<Option<Arc<VerifiedChain>>>
tokio::spawn(index.published().gate(verified.clone(), finalised_depth, cancel.child_token()));
tokio::spawn(index.run(blocks)); // infallible: returns at Shutdown, panics on any failure

let producer = Producer::new(block_sink, sources, verified, concurrency, durable);
tokio::spawn(producer.run(cancel.child_token()));
```

## `Producer`

`Producer::new(sink, sources, verified, concurrency, durable)` takes the sink
(at least one subscriber), every source that serves blocks by hash (any
`ChainDataSource`; none is trusted, every block is checked), the header
chain's `watch<Option<Arc<VerifiedChain>>>` (`HeaderSync::subscribe`; `None`
= nothing verified yet, waited out), how many blocks may be in flight, and
every subscriber's durable tip (last block on disk, height + hash, `None` when
empty). It follows that chain and nothing else, decides every step the sink
carries, and is the only place chain identity is checked
([verified-chain.md §9](../../docs/design/verified-chain.md#9-the-producer)).
The full block-sink contract (finality, reorgs, retreats, a reorg and
finality in one update) is in
[data-sink.md](../../docs/design/data-sink.md#how-the-producer-publishes).

- **start** = after the rearmost `durable`: **every** subscriber receives the
  same contiguous block sequence from there, so an index asserts on height
  instead of tolerating gaps. An index ahead of the rearmost (a crash between
  two indexes' commits, a newly enabled index on a synced node) receives
  heights it already holds, final, and skips them
- **restart check**: nothing is sent until the chain's final tip covers every
  `durable` tip and each is the chain's block at its height; one that is not
  stops production with `ProduceError::Diverged` (a directory from another
  chain, a reset header store on another chain): resync required
- **fetch**: each wanted `(height, hash)` goes to the least-loaded source as
  `get_block_by_hash` on its own task; `concurrency` blocks in flight ahead
  of the next one sent, sent in height order
- **check**: a block is accepted only if its hash = `hash_at(height)`, its
  coinbase height = the height, and the merkle root rebuilt from its txids =
  `header_at(height)`'s (a repeated txid pair refused too, CVE-2012-2459).
  Anything else = that source misanswered (WARN `Source misanswered a block`),
  never an invalid header: the source is skipped for 60 s and another asked.
  A source silent for 15 s is hedged with another; a block every source
  failed is asked again after 1 s
- **finality**: one, the chain's final tip. Each block is a `Step::Apply`
  whose `finalized` says whether it is at or below it; each non-final one
  delivered gets a `Step::Finalized` once the final tip passes it, oldest
  first. The best tip itself is not a step: an index's serving gate reads the
  same chain (see `Published::gate`)
- **reorg**: the first delivered non-final block that is no longer
  `hash_at` its height (above the best counts: a retreat) is the fork →
  `Step::Reorg`, then the still-best blocks below it again from memory (no
  fetch), then the new branch (final is never resent)
- header sync gone → `ProduceError::ChainGone`; cancel → `Ok`; either way it
  ends the sink with `shutdown()`, so every index loop writes what is final
  and stops

Inside, a pure `ProducerCore` (`step(input, now) -> outputs`: chain, answer or
tick in; sink steps and fetches out; no I/O, no clock) under a thin async
driver. `ProducerCore::check()` asserts P1–P6 after every step in tests and
debug builds; its model test and fire drills live beside it.

Metrics (`describe_metrics()`): `zaino_best_tip`,
`zaino_reorgs_total`, `zaino_fetch_height`, the per-block
`zaino_fetch_*_total` counters, and `zaino_sink_queue_bytes` (see below).

## `IndexerDataSink<T>`

A broadcast queue that decides nothing: start, finality and resets are the
steps its publisher sends. The concepts, a picture and a step-by-step reorg are
in [the data sink](../../docs/design/data-sink.md); this section is the API.

- `IndexerDataSink::new(name)`, then `subscribe(name, budget)` per subscriber →
  a `Subscription<T>` (its queue); the publisher takes the sink by value, so
  nobody subscribes mid-stream
- `send(step)` → the same step to every queue, in order (one `Arc<T>` shared)
- `shutdown()` consumes the sink: `Step::Shutdown` last in every queue, past
  a full budget (never waits); a subscription pops `Shutdown` again on every
  later call

`Step` = `Apply { height, finalized, data }`, `Finalized { height }`,
`Reorg`, `Shutdown`: blocks and what happens to them, never the chain tip.

`send`, `shutdown` and `next` are infallible. Their one panic is the failure
path ([Failure](#failure-panic-never-err)): a queue dropped before it pops
`Shutdown` panics the publisher's next `send`, and a sink dropped without
`shutdown()` panics the subscriber's next pop.

### Backpressure

One `Arc` per item, N byte-bounded queues (`budget` bytes per subscriber).
`T: Weight` names what an item holds in memory (`Block` → `Block::footprint()`),
and each queued step holds that many bytes of its subscriber's budget until
popped. Budget in bytes, not items: slack stays steady whether blocks are 1 KB
or 2 MB. A step heavier than the whole budget still passes, alone, once the
queue drains, so no item can wedge the pipeline. Delivery is serial across
subscribers: a full queue delays the rest, which is what bounds memory. A
persistently slow index eventually paces the producer.

`zaino_sink_queue_bytes{sink, subscriber}` shows which index that is: the
bytes each queue holds, added on `send` and subtracted on pop (the exact
permits, not a sample). It is labelled by sink as well, because one index can
subscribe to two sinks (compact-block reads `blocks` and `fees`). A gauge
sitting at its subscriber's budget means that index is holding back the
producer. A step heavier than the budget counts as the budget, which is what it
holds. `Shutdown` counts nothing.

### An index publishing to another

An index that derives per-block data another index needs sends it into a plain
`IndexerDataSink<Item>` from its own loop: every step it follows, 1:1. Each
`Apply` carries the derived item and the upstream's `finalized` flag (heights
it already holds included: a downstream index behind it still needs them),
each `Finalized` and `Reorg` is forwarded, and `shutdown()` ends the sink after
the upstream `Shutdown`. The stream is the upstream's, step for step.

A consumer awaits one step off each queue per step: `blocks.next().await`,
then `fees.next().await`. The publisher's loop guarantees they line up, so the
two `Shutdown`s arrive together and any mismatch is asserted. A publisher that
fails panics without `shutdown()`, so the consumer's next pop panics too.

`FeeSink` = `IndexerDataSink<BlockFees>`, published by
[`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md) and
read by the compact-block index.

## An index loop

Every index's `run` is the same loop over its subscription. Nothing drives it
and nothing hides it; the arms are the whole policy:

An index's storage tiers are `zaino_persistence::Tiered`
([persistence-engine.md §5](../../docs/design/persistence-engine.md#5-tiering)):
the loop encodes each block into one `Changes` and maps steps onto
`stage` / `apply` / `finalize` / `reorg`:

```rust,ignore
pub async fn run(mut self, mut blocks: Subscription<Block>) {
    loop {
        match blocks.next().await {
            Step::Apply { height, finalized: true, data } => {
                if Some(height) <= self.durable_tip().map(|tip| tip.height) {
                    continue;                                   // replay: already on disk
                }
                let full = self.tiered.get_mut().stage(self.changes(&data), data.weight());
                self.published.merged(height);
                if full {
                    self.finalize(height).await;                // one bulk batch = one fsync
                }
            }
            Step::Apply { finalized: false, data, .. } => {
                self.finalize_staged().await;                   // bulk → tip handoff
                self.tiered.get_mut().apply(self.changes(&data));
                self.publish();
            }
            Step::Finalized { height } => self.finalize(height).await,
            Step::Reorg => { /* tiered.reorg(), re-derive any carry, publish, reorged() */ }
            Step::Shutdown => return self.finalize_staged().await,
        }
    }
}
```

`finalize(through)` commits every held block through `through` as one
`Changes` (`Offloaded::blocking`: the tiers hop to the blocking pool and back),
publishes the view, then `Published::durable(height)`. It returns once the
blocks are on disk: one write at a time, nothing in flight. Like `run`, it
returns `()`: a failed write panics inside `Tiered::finalize`
([Failure](#failure-panic-never-err)). Each index keeps its loop in
`src/index_writer.rs`.

An index that batches its derivation (value-balance's prevout probe,
tree-state's Merkle hashing) takes `Subscription::run(first, budget)` on an
`Apply`: `first` plus every `Apply` already queued behind it, up to `budget`
bytes, never a wait; the step that ended the run is the next `next()`.

Commit cadence follows from the arms. In bulk, final blocks stage until a
batch fills. At the tip, every `Finalized` commits at once: the durable tip
trails the chain tip by exactly the finality depth. The bulk → tip handoff
commits what was staged before the first non-final block applies on it.
Indexes do no chain-identity checks: the producer checks every block against
the verified header chain (hash, body) and every index's durable tip against
its final part.

A reorg and a restart are the same operation: every applied block is dropped
and blocks re-apply from the durable tip. `Step::Reorg` carries no height, so
no index owns a reverse fold. Blocks arrive strictly contiguous and ascending;
`Tiered` asserts it on every block, because cumulative state (tree sizes, note
positions, balances) goes plausibly wrong after a gap. See
[`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).


### Off the runtime

An index loop awaits data, computes, and awaits the write, and never picks a
thread:

```rust,ignore
use zaino_sync::{compute, Offloaded};

// fold on the CPU pool onto the carry, then commit on the blocking pool; each state comes back
let changes = self.carries.compute(move |carries| carries.fold(&blocks, &view)).await?;
self.tiered.blocking(move |tiered| tiered.finalize(through)).await;
```

| helper | runs `f` on | for |
|---|---|---|
| `compute(f)` | the rayon pool, one thread per core | real CPU work: hashing, cryptography |
| `blocking(f)` | tokio's blocking pool | blocking syscalls and mmap faults: `pwrite`, `fsync`, cold reads |

- Cheap bookkeeping (projecting a block into rows, encoding a record, map
  inserts) stays inline on the loop: a hop costs more than it saves.
- Inside `compute`, `rayon::par_iter` / `par_chunks` spreads data-parallel work
  over the same pool (the tree-state fold hashes each tree level across every
  core this way).
- `Offloaded<S>` holds index-owned state: `compute(f)` / `blocking(f)` move it
  to the pool and back within one call. `get()` panics while it is out, never
  waits.
- A panic in either resumes on the awaiting task: zainod aborts rather than keep
  a half-built state.
- Fetch and index writes overlap: while an index commits, the producer keeps
  fetching into its subscription queue (`queue_mib`).

### `Published`: what serving, metrics and status read

`Published::new(view, durable)` at boot (`durable` = the durable tip block);
the loop calls `view(v, applied)` after every step (view + applied block,
height and hash, published together), `durable(h)` on
each landing (**after** it is on disk: nothing is told of a height not
written; never moves back), and `reorged()` after publishing the dropped view.

- `served()` = `Served<View>`, what a service holds: `pin()` = the latest view
  (one load per request), `None` while the gate is closed; `pin_any()` = the
  view regardless (final data, tips; what tests assert on); `synced()` = the
  gate without pinning. `Served::fixed(view)` = a synced, never-republished
  handle
- `reads()` = every view a `served()` handle pinned, shared across clones
- `subscribe_finalized()` / `subscribe_applied()` (a `BlockRef`) /
  `subscribe_synced()` = watches for metrics and the status report (and tests:
  `wait_for`)
- `gate(verified, depth, cancel)` = the serving gate as its own task, off the
  loop, over the header chain's `watch<Option<Arc<VerifiedChain>>>`: it opens
  once the applied block **is** the verified best (hash, not height), and
  closes when the applied block leaves the best chain, falls more than `depth`
  behind it (a producer stalled while the chain moved on), or on a reorg until
  the replay is back at the best. Each flip logs
  `Serving` / `Syncing, requests refused`, or around a reorg `Reorg received,
  requests refused until replayed` / `Reorg replayed, serving` (`took`)

### Testing an index

Drive it as production does: send `Step`s into a real `BlockSink` (and
`FeeSink`), `tokio::spawn(index.run(subscription))`, and assert through
`published()` (`served().pin_any()`, `subscribe_finalized().wait_for(..)`).
`.await` on the handle = `Ok(())` after `Shutdown`; a failure is its
`JoinError::into_panic()`, asserted on its message. Reopen the store after `run`
returns for durability. No index exposes a test-only entry point.

### Shutdown

The `Producer` owns the `BlockSink`; when it stops, `Step::Shutdown` goes last
into every queue. An index stops there: it writes what is final, ends any sink
it publishes to with `Shutdown`, and returns. A step that reached a queue is
never dropped.

### Failure: panic, never `Err`

`run`, `commit` and every step in between are infallible by signature. An index
cannot recover from a failed write (an `fsync` error is never retried,
[durability.md](../../docs/design/durability.md) §6), so an `Err` would only be
carried to the same exit. Every failure panics where it happens instead:

| Failure | Panic message |
|---|---|
| commit hits a full disk (`StorageFull` / `QuotaExceeded` anywhere in the error chain) | `<index> index commit failed: disk <dir> full` |
| any other commit error | `<index> index commit failed at <dir>: <error>` |
| chain data the index cannot take (an unrecorded prevout, a tree size past `u32`, a non-canonical commitment) | `<index> index: <error>` |
| a broken invariant (a gap, a reorg with blocks staged, out-of-step fees) | the `assert!` message, prefixed with the index |

`Tiered::finalize` (in `zaino-persistence`) panics with both commit messages,
so every index reports a failed commit the same way.

In zainod the panic hook logs the panic as an `error` event and aborts the
process at once. The service manager restarts it, and every index reopens at its
last durable manifest. Without that hook (tests, embedders) the panic stops the
pipeline through the sink, with no cancel token and no draining: the failed
index's queues drop, the producer's next `send` panics, and a downstream consumer
(compact-block, after value-balance) panics on its next pop.

The only fallible calls are the ones made at boot, before the pipeline exists:
`open` / `new` return `StoreError` or the index's `IndexWriterError`, and
`zainod start` exits 1 with that error.

`zainod::indexer`'s module docs diagram the full pipeline;
[`docs/design/persistence-architecture.md`](../../docs/design/persistence-architecture.md)
carries the measurements behind these choices.
