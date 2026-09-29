# `zaino-sync` — usage

Zaino's index sync pipeline: `Producer` → `BlockSink` → subscribed
`IndexWriter`s, each driven by its own `IndexFollower`. Design:
[`docs/design/sync.md`](../../docs/design/sync.md).

- **Produce**: `Producer` (the one task that owns the `BlockSink`) bulk-fetches
  through `zaino_source::BlockFetchPool`, then follows `zaino-chainview`'s
  quorum tip through `zaino_chain_head::ChainHead`
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

let mut blocks = BlockSink::new(finalised_depth);
// each index reports its OWN durable extent, before the producer starts
let subscription = blocks.subscribe(MyIndex::NAME, queue_bytes, writer.finalized_height());
let follower = IndexFollower::new(writer, subscription, batch_bytes);
// take served() / subscribe_synced() / subscribe_finalized() for serving, then:
tokio::spawn(follower.run()); // ends once the producer (the sink's owner) is dropped

let producer = Producer::new(blocks, pool, chainview.subscribe_tip());
tokio::spawn(producer.run(cancel.child_token()));
```

## `Producer`

`Producer::new(sink, pool, tips)` takes the sink (at least one subscriber,
`finalised_depth` > 0), a `BlockFetchPool` over every validator, and
chainview's level-triggered quorum tip (`watch<Option<QuorumTip>>`; `None` =
quorum lost, waited out).

- **bulk**: `next ..= tip − finalised_depth` streamed from the pool (ordered,
  concurrent, decoded on every core), every block final, each `prev_hash`
  checked against the block before it; the chain head anchors on the last one.
  The end follows the quorum tip mid-pass (`set_tip` before each raise, so
  every bulk block stays final): one pass per catch-up. An index ahead of the
  validators waits for them
- **live**: each quorum tip → `ChainHead::advance` → `set_tip` + `add` the new
  blocks; a reorg → `reset`, then `add` from the resume height out of the
  window (no fetch; the resume height is asserted ≤ the fork)
- a quorum tip more than `finalised_depth` ahead → bulk again
- a fetch failure (validators failed after the pool's retries), bulk or live →
  WARN, retry after 1 s (bulk resumes from what was added); an unlinked bulk block, a fork
  below the window or chainview gone → `ProduceError`, the task ends (zainod
  exits)
- cancel → `Ok`; returning drops the sink, so every follower drains and
  flushes what is final

Metrics (`prometheus` feature, `describe_metrics()`): `zaino_best_tip`,
`zaino_reorgs_total`, `zaino_fetch_height` and the per-block
`zaino_fetch_*_total` counters.

## `IndexerDataSink<T>`

### One resume point

`subscribe(name, budget, durable)` returns a `Subscription<T>`: the
subscriber's `Step<T>` queue plus the tip (its follower's serving gate).
Production starts at the rearmost `durable` extent (`next()`), and **every**
subscriber receives the same contiguous block sequence from there, so an index asserts on
height instead of tolerating gaps. An index ahead of the rearmost (a crash
between two indexes' commits, a newly enabled index on a synced node) receives
heights it already holds, and skips them in `IndexWriter::deliver`. It still
sees them, because an index feeding another sink must republish them.

- `set_tip(tip)` → the final boundary = highest tip + 1 − `finalised_depth`,
  never lowered; a `Step::Finalized { height }` for each delivered non-final
  height it buries, oldest first
- `finalize_through(extent)` → the same without a tip, for a producer that
  learns finality from its own upstream (the value-balance index marks what it
  has made durable)
- `add(height, Arc<T>)` → `Step::Apply { height, finalized, data }` to every
  subscriber; contiguity asserted
- `reset()` → `Step::Reset` down every queue, rewound to the first non-final
  height; returns where the producer resumes (final is never resent)
- dropping the sink closes every queue (subscribers drain and stop)

### Backpressure

One `Arc` per item, N byte-bounded queues (`budget` bytes per subscriber).
`T: Weight` names what an item holds in memory (`Block` → `Block::footprint()`),
and each queued step holds that many bytes of its subscriber's budget until
popped. Budget in bytes, not items: slack stays steady whether blocks are 1 KB
or 2 MB. A step heavier than the whole budget still passes, alone, once the
queue drains, so no item can wedge the pipeline. Delivery is serial across
subscribers: a full queue delays the rest, which is what bounds memory. A
persistently slow index eventually paces the producer.

### Derived sinks

An index that derives per-block data another index needs publishes it into its
own `IndexerDataSink<U>` from `deliver`, one item per block. The consumer keeps
its lifecycle on `BlockSink` and pairs each block with its item by height and
hash; it never drives its writer from the derived stream.

`ValueBalanceSink` = `IndexerDataSink<BlockValueBalances>`, published by
[`zaino-internal-value-balance`](../zaino-internal-value-balance/usage.md) and
read by the compact-block index:

```rust,ignore
let mut balances = ValueBalanceSinkBuilder::new(finalised_depth);
let for_compact = balances.subscribe("compact_block", queue_bytes, compact_durable);
let publisher = ValueBalanceIndexWriter::open(fs, &path, network, balances.seal())?;

// consumer, in `deliver`:
let fees = for_compact.balances_for(&block).await; // None = the publisher stopped
```

`balances_for` skips `Finalized`/`Reset` steps and items a reorg left queued
(another branch's hash).

## Follow

### `IndexWriter`: two extents

| method | contract |
|---|---|
| `finalized_height()` | durable extent, one past the highest height on disk; resume point; never moves backwards |
| `finalized_tip()` | hash of the durable tip as committed (`None` iff empty): the chain identity the next delivered block must link onto |
| `applied_height()` | pre-commit extent, the height the next `apply` expects |
| `view()` | cheap-to-clone snapshot of every tier (`type View`), published after every step and commit |
| `deliver(&[Arc<Input>])` | every delivered item, in order, before any of the run is staged or applied (heights inside `finalized_height()` included, and only here); a run = the steps already queued (one item at the tip, up to `batch_bytes` in bulk), so per-item lookups batch across it; default = nothing |
| `apply(&Arc<Input>)` | fold one item into pre-commit (not durable) |
| `finalize(&[Arc<Input>])` | prepare final items, contiguous from `finalized_height()`, and return the `write` that stores them (an owned `FnOnce() -> Result<Done, Error>` holding the lent store); nothing durable, nothing dropped from pre-commit |
| `committed(Done)` | land a finished `write`: store back, `finalized_height()` moves, written items leave pre-commit, downstream told; must also advance `applied_height()` to at least the new `finalized_height()` |
| `reset()` | drop **all** pre-commit so `applied_height() == finalized_height()`; never touches durable state |

A write is out between `finalize` and `committed`, and the follower keeps
delivering and applying meanwhile, so every other method answers without the
store: durable extent, tip and read snapshots as of the last `committed`. Every
tier change lands in `committed`, so a reader sees an item in exactly one tier
(pre-commit until the landing, durable after), never both, never neither.
`zaino_sync::finalize_now(writer, items)` = all three in one await, for tests
and offline rebuilds.

`type Input: Linked + Weight` is the item of the sink the index subscribes to
(`Block` for `BlockSink`); `Linked` = `height()`, `hash()`, `prev_hash()`;
`Weight` = the bytes it holds (sizes both the queue budget and the commit batch).

`apply` receives strictly contiguous, ascending heights. Assert it at the top
of every implementation: an index threading cumulative state (tree sizes, note
positions, balances) produces plausible but wrong output after a gap, and a
panic is far cheaper.

A reorg and a restart are the same operation: pre-commit is dropped and blocks
are re-applied from the durable tip. `Step::Reset` carries no height, so no
index owns a reverse fold.

`apply`, `finalize` and `reset` run on a runtime worker. Anything slow goes
through one of two helpers (see [Off the runtime](#off-the-runtime)), or it
stalls the producer feeding every index.

See [`docs/design/precommit-state.md`](../../docs/design/precommit-state.md).

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

`IndexFollower::new(writer, subscription, batch_bytes)`; `run(self) -> Result<(), FollowError<W::Error>>`.

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

- `Apply { finalized: true }` skips pre-commit and is staged straight for
  `finalize` (bulk sync = one fold per block); a non-final one is `apply`d and
  staged when its `Finalized` arrives
- `finalize` runs once the staged items' `Weight` reaches the follower's
  `batch_bytes` (`IndexFollower::new(writer, subscription, batch_bytes)`), so one
  write = one fsync of a steady size however big each block is; at the tip every
  final item commits as it arrives
- its write runs on the blocking pool while the follower keeps going; at most
  one is out: the next `finalize`, the bulk → tip handoff, a `Reset` and the
  final stop each wait for it to land first. A write finishing while the loop
  waits for input lands at once (durability published as it happens)
- `Reset` writes what is staged (final is final), then `reset`s pre-commit
- `subscribe_finalized()` publishes the durable extent **after** it is durable;
  anything gating on it is never told about a height that is not on disk. Each
  index publishes its own; a query spanning indexes reads at the minimum
- `served()` = `Served<View>`, what an index's service holds: `pin()` = the
  latest published `View` (one load per request), `None` while unsynced;
  `pin_any()` = the view regardless (final data, extents). `Served::fixed(view)`
  = a synced, never-republished handle for tests with no follower
- `subscribe_synced()` is `true` only while `applied_height() > tip`, recomputed
  when the queue is idle and dropped immediately on reset; serving refuses every
  request of that index while `false`; each flip logs `index serving gate`

### Shutdown drains

The `Producer` owns the `BlockSink`; when it stops, the queues close. A
follower takes no cancellation token; it stops when its subscription closes:
it drains what is queued, finalises what is final, and returns. A step that
reached a queue is never dropped.

`zainod::indexer`'s module docs diagram the full pipeline;
[`docs/design/persistence-architecture.md`](../../docs/design/persistence-architecture.md)
carries the measurements behind these choices.
