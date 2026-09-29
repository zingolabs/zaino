# `zaino-sync` — usage

Zaino's index sync pipeline: `Producer` → `BlockSink` → subscribed
`IndexWriter`s, each driven by its own `IndexFollower`. Design:
[`docs/design/sync.md`](../../docs/design/sync.md).

- **Produce**: `Producer` (the one task that owns the `BlockSink`) bulk-fetches
  through `zaino_source::BlockFetchPool`, then follows `zaino-chainview`'s
  quorum tip through `zaino_non_finalized_state::ChainHead`
- **Stream**: `IndexerDataSink<T>` (one producer, N subscribers, keyed by block
  height); `BlockSink` = `IndexerDataSink<Block>`, the one stream every index
  subscribes to
- **Follow**: the `IndexWriter` trait an index implements, and `IndexFollower`,
  which drives it from its `Subscription` as one spawned task

No scheduler, no dependency graph, no storage. Each index owns its files, its
cumulative state, its commit cadence and both of its heights; this crate owns
the loops.

```rust,ignore
use zaino_sync::{BlockSink, IndexFollower, IndexWriter, Producer};

let mut block_sink = BlockSink::new("blocks");
let subscription = block_sink.subscribe(MyIndex::NAME, queue_bytes);
let tips = chainview.subscribe_tip();
let follower = IndexFollower::new(writer, subscription, tips.clone(), batch_bytes, finalised_depth);
let durable = [follower.writer().finalized_height()]; // every subscriber's durable tip
// take served() / subscribe_synced() / subscribe_finalized() for serving, then:
// ends at the producer's Shutdown; a failure cancels `cancel` (the whole pipeline) first
tokio::spawn(follower.run(cancel.clone()));

let producer = Producer::new(block_sink, pool, tips, finalised_depth, durable);
tokio::spawn(producer.run(cancel.child_token()));
```

## `Producer`

`Producer::new(sink, pool, tips, depth, durable)` takes the sink (at least one
subscriber), a `BlockFetchPool` over every validator, chainview's
level-triggered quorum tip (`watch<Option<QuorumTip>>`; `None` = quorum lost,
waited out), the reorg depth, and every subscriber's durable tip (last height
on disk, inclusive, `None` when empty). It decides every step the sink carries:

- **start** = after the rearmost `durable`: **every** subscriber receives the
  same contiguous block sequence from there, so an index asserts on height
  instead of tolerating gaps. An index ahead of the rearmost (a crash between
  two indexes' commits, a newly enabled index on a synced node) receives
  heights it already holds, and skips them in `IndexWriter::deliver`
- **finality**: the final tip (last final height, inclusive) = highest tip −
  depth, never lowered, and at least the furthest `durable` (a lower tip after
  a restart cannot un-finalise what an index holds). Each new tip sends a
  `Step::Finalized` for every delivered non-final height it buries, oldest
  first; each block is a `Step::Apply` whose `finalized` says whether it is
  already final. The tip itself is not a step: a follower reads it off
  chainview (see `IndexFollower`)
- **reset**: `Step::Reset`, then a replay from the first non-final height
  (final is never resent)

- **bulk**: `next` to `tip − finalised_depth`, both inclusive, streamed from the pool (ordered,
  concurrent, decoded on every core), every block final, each `prev_hash`
  checked against the block before it; the chain head anchors on the last one.
  The end follows the quorum tip mid-pass (finality moves before each raise, so
  every bulk block stays final): one pass per catch-up. An index ahead of the
  validators waits for them
- **live**: each quorum tip → `ChainHead::advance` → the `Finalized` it buries
  + an `Apply` per new block; a reorg → `Reset`, then the `Apply`s from the resume height out of
  the window (no fetch; the resume height is asserted ≤ the fork)
- by-height fetches (bulk, and the live extension) go only to the validators
  in the quorum tip's `agreed_by` (`BlockFetchPool::among`). They agree on the
  tip's hash, so they agree on every height below it. Another validator may
  still serve a stale branch at a height the new tip has made final, and
  publishing that block would finalize it in every index
- a quorum tip more than `finalised_depth` ahead → bulk again
- a fetch failure (validators failed after the pool's retries), bulk or live →
  WARN, retry after 1 s (bulk resumes from what was added); an unlinked bulk block, a fork
  below the window or chainview gone → `ProduceError`, the task ends (zainod
  exits)
- cancel → `Ok`; either way it ends the sink with `shutdown()`, so every
  follower flushes what is final and stops

Metrics (`describe_metrics()`): `zaino_best_tip`,
`zaino_reorgs_total`, `zaino_fetch_height`, the per-block
`zaino_fetch_*_total` counters, and `zaino_sink_queue_bytes` (see below).

## `IndexerDataSink<T>`

A broadcast queue that decides nothing: start, finality and resets are the
steps its publisher sends.

- `IndexerDataSink::new(name)`, then `subscribe(name, budget)` per subscriber →
  a `Subscription<T>` (its queue); the publisher takes the sink by value, so
  nobody subscribes mid-stream
- `send(step)` → the same step to every queue, in order (one `Arc<T>` shared)
- `shutdown()` consumes the sink: `Step::Shutdown` last in every queue, past
  a full budget (never waits); a subscription pops `Shutdown` again on every
  later call

`Step` = `Apply { height, finalized, data }`, `Finalized { height }`,
`Reset`, `Shutdown`: blocks and what happens to them, never the chain tip.
None of these fail. A subscriber holds its
queue until it pops `Shutdown`, so a queue dropped before then panics the sink,
and a sink dropped without `shutdown()` (its publisher panicked) panics the
subscriber.

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

An index that derives per-block data another index needs implements
`Derives` (`type Item`, `derive(&blocks)` = one item per block of the run just
delivered), and its follower republishes into a plain
`IndexerDataSink<Item>`: `IndexFollower::new(..).publishing(sink)`. The
follower sends **every** step it follows, 1:1: each `Apply` with the derived
item and the upstream's `finalized` flag, each `Finalized` and `Reset`,
and `Shutdown` last (on a clean stop and on a failure alike). So the stream is
the upstream's, step for step.

A consumer reads it in lockstep beside its own block subscription:
`Zip::new(block_subscription, derived_subscription)` is the follower's feed,
one step off each stream per step, asserted to be the same step (same kind,
height, `finalized`, and the item derived from that block). It yields
`Paired { upstream, derived }`. A derived `Shutdown` ahead of the upstream's
means the publisher failed: the zip ends (`Shutdown`) and pops the upstream
through its own `Shutdown`; the publisher reports the failure. Any other
mismatch panics.

`FeeSink` = `IndexerDataSink<BlockFees>`, published by
[`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md) and
read by the compact-block index (`BlockWithFees` = its paired item):

```rust,ignore
let mut fee_sink = FeeSink::new("fees");
let feed = Zip::new(
    block_sink.subscribe("compact_block", queue_bytes),
    fee_sink.subscribe("compact_block", queue_bytes),
);
let compact =
    IndexFollower::new(CompactBlockIndexWriter::new(store), feed, tips.clone(), batch_bytes, depth);
let fees =
    IndexFollower::new(value_balance_writer, fees_subscription, tips.clone(), batch_bytes, depth)
        .publishing(fee_sink); // takes the sink: no subscriber after this
```

## Follow

### `IndexWriter`: two tips

Every height here is a last height, inclusive, with `None` meaning nothing
held.

| method | contract |
|---|---|
| `finalized_tip()` | the last durable block (`BlockRef`) as committed, `None` when empty: the resume point, never moves backwards, and the chain identity the next delivered block must link onto |
| `finalized_height()` | provided: `finalized_tip()`'s height |
| `applied_height()` | the last applied height (non-finalized state included); the next `apply` expects the height after it |
| `view()` | cheap-to-clone snapshot of every tier (`type View`), published after every step and commit |
| `deliver(&[Arc<Input>])` | every delivered item, in order, before any of the run is staged or applied (heights at or below `finalized_height()` included, and only here); a run = the steps already queued (one item at the tip, up to `batch_bytes` in bulk), so per-item lookups batch across it; default = nothing |
| `apply(&Arc<Input>)` | fold one item into the non-finalized state (not durable) |
| `finalize(&[Arc<Input>])` | prepare final items, contiguous from the height after `finalized_height()`, and return the `write` that stores them (an owned `FnOnce() -> Result<Done, Error>` holding the lent store); nothing durable, nothing dropped from the non-finalized state |
| `committed(Done)` | land a finished `write`: store back, `finalized_height()` moves, written items leave the non-finalized state, downstream told; must also advance `applied_height()` to at least the new `finalized_height()` |
| `reset()` | drop **all** non-finalized state so `applied_height() == finalized_height()`; never touches durable state |

A write is out between `finalize` and `committed`, and the follower keeps
delivering and applying meanwhile, so every other method answers without the
store: durable tip and read snapshots as of the last `committed`. Every
tier change lands in `committed`, so a reader sees an item in exactly one tier
(non-finalized until the landing, durable after), never both, never neither.
`zaino_sync::finalize_now(writer, items)` = all three in one await, for tests
and offline rebuilds.

`type Input: Linked + Weight` is the item of the feed the index follows
(`Block` for a `BlockSink` subscription, `Paired<A, B>` for a `Zip`: linked
through its block); `Linked` = `height()`, `hash()`, `prev_hash()`;
`Weight` = the bytes it holds (sizes both the queue budget and the commit batch).

`apply` receives strictly contiguous, ascending heights. Assert it at the top
of every implementation: an index threading cumulative state (tree sizes, note
positions, balances) produces plausible but wrong output after a gap, and a
panic is far cheaper.

A reorg and a restart are the same operation: the non-finalized state is
dropped and blocks are re-applied from the durable tip. `Step::Reset` carries no height, so no
index owns a reverse fold.

`apply`, `finalize` and `reset` run on a runtime worker. Anything slow goes
through one of two helpers (see [Off the runtime](#off-the-runtime)), or it
stalls the producer feeding every index.

See [`docs/design/non-finalized-state.md`](../../docs/design/non-finalized-state.md).

### Off the runtime

An index writer awaits data, computes, and awaits the write, and never picks a
thread:

```rust
use zaino_sync::{compute, Offloaded};

// finalize: fold on the CPU pool, then hand the lent store to the write
let chunk = compute(move || fold(&blocks)).await;
let mut store = self.store.lend();
Ok(move || { store.write(&chunk)?; Ok(Landing { store }) })   // follower runs it off-loop

// committed: the store comes back
self.store.restore(landing.store);
```

| helper | runs `f` on | for |
|---|---|---|
| `compute(f)` | the rayon pool, one thread per core | real CPU work: hashing, cryptography |
| `blocking(f)` | tokio's blocking pool | blocking syscalls and mmap faults: `pwrite`, `fsync`, cold reads |

- Cheap bookkeeping (projecting a block into rows, encoding a record, map
  inserts) stays inline on the writer task: a hop costs more than it saves.
- Inside `compute`, `rayon::par_iter` / `par_chunks` spreads data-parallel work
  over the same pool (the tree-state fold hashes each tree level across every
  core this way).
- `Offloaded<S>` holds writer-owned state: `compute(f)` moves it to the pool
  and back within one call; `lend()` / `restore(s)` hand a store to a `finalize`
  write and take it back in `committed`. `get()` panics while it is out, never
  waits: no lock sits beside it.
- A panic in either resumes on the awaiting task: zainod aborts rather than keep
  a half-built state.
- Fetch, delivery and index writes overlap: while a write is out, the follower
  keeps delivering, applying and staging the next batch, and the producer keeps
  fetching into the subscription queue (`queue_mib`).

### `IndexFollower`

`IndexFollower::new(writer, feed, tips, batch_bytes, depth)` (feed = a
`Subscription` or a `Zip`; `tips` = chainview's quorum tip,
`watch<Option<QuorumTip>>`; `depth` = how far behind that tip the serving gate
stays open), `.publishing(sink)` for a `Derives` writer;
`run(self, shutdown: CancellationToken) -> Result<(), FollowError<W::Error>>`.

The chain tip is not a sink step. The follower reads it off `tips` for two
decisions, both taken when its queue is empty:

- the serving gate opens once the index has applied the tip, and closes when it
  falls more than `depth` behind (or on a `Reset`, until the replay is back)
- at the tip, each final block commits as it arrives; below it, commits wait
  for a full batch

A tip that moves with no block behind it (the producer stalled while the
validators moved on) wakes the follower too, so the gate closes rather than
serving stale data as current. If chainview's sender drops, the follower keeps
following steps through `Shutdown`.

- every delivered block must link onto the one before it, the first onto
  `finalized_tip()` when it extends it (and again after each `Reset`). A break
  is `FollowError::Unlinked { index, height, expected, got }`: the validator's
  chain diverged below the durable tip (a reorg past the window, a reset
  validator, a directory from another chain), so the index stops rather than
  fold a foreign block onto its state. Resync required
- blocks replayed from below the durable tip must land on it: the block at
  `finalized_height().last()` must hash to `finalized_tip()`, or
  `FollowError::Diverged { index, height, expected, got }`
- heights inside `finalized_height()` go to `deliver` only, never staged
- a writer failure is `FollowError::Index { index, source }`; zainod exits on
  either

- `Apply { finalized: true }` skips the non-finalized state and is staged straight for
  `finalize` (bulk sync = one fold per block); a non-final one is `apply`d and
  staged when its `Finalized` arrives
- `finalize` runs once the staged items' `Weight` reaches the follower's
  `batch_bytes` (`IndexFollower::new`'s third argument), so one
  write = one fsync of a steady size however big each block is; at the tip every
  final item commits as it arrives
- its write runs on the blocking pool while the follower keeps going; at most
  one is out: the next `finalize`, the bulk → tip handoff, a `Reset` and the
  final stop each wait for it to land first. A write finishing while the loop
  waits for input lands at once (durability published as it happens)
- `Reset` writes what is staged (final is final), then `reset`s the non-finalized state
- `subscribe_finalized()` publishes the durable tip height (inclusive, `None`
  when empty) **after** it is durable;
  anything gating on it is never told about a height that is not on disk. Each
  index publishes its own; a query spanning indexes reads at the minimum
- `served()` = `Served<View>`, what an index's service holds: `pin()` = the
  latest published `View` (one load per request), `None` while unsynced;
  `pin_any()` = the view regardless (final data, tips). `Served::fixed(view)`
  = a synced, never-republished handle for tests with no follower
- `subscribe_synced()` is `true` only while `applied_height() > tip`, recomputed
  when the queue is idle and dropped immediately on reset; serving refuses every
  request of that index while `false`; each flip logs `index serving gate`

### Shutdown drains

The `Producer` owns the `BlockSink`; when it stops, `Step::Shutdown` goes last
into every queue. A follower stops there: it finalises what is final, ends its
derived sink (if it publishes one) with `Shutdown`, and returns. A step that
reached a queue is never dropped.

`run(shutdown)` takes the pipeline's root `CancellationToken` only to raise it.
A follower that fails cancels it, keeps popping every queue of its feed through
`Shutdown` (the sink never sees a dropped queue), ends its derived sink, then
returns its error. The failure is the only error: nothing upstream reports a
dead consumer, and a zip consumer of a failed publisher stops cleanly.

`zainod::indexer`'s module docs diagram the full pipeline;
[`docs/design/persistence-architecture.md`](../../docs/design/persistence-architecture.md)
carries the measurements behind these choices.
