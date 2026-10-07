# The data sink: the final stream

`IndexerDataSink<T>` is the plumbing between the non-finalized state (`zaino-nfs`) and every
index writer: one publisher, N subscribers, each with its own byte-bounded queue. The one sink in
the daemon carries `Final` steps: every final block exactly once, in height order, never
retracted. Reorgs never reach it: they live in the NFS ([nfs.md](./nfs.md)).

```text
 header chain (VerifiedChain) ──┐
                                ├─▶ Nfs ──▶ final stream ─┬─▶ [≤ queue_mib] ─▶ value-balance ─▶ FeeSink ─┐
 any source (getblock <hash>) ──┘  (checked, folded)      ├─▶ [≤ queue_mib] ─▶ compact-block ◀───────────┘
                                                          ├─▶ [≤ queue_mib] ─▶ tree-state
                                                          └─▶ [≤ queue_mib] ─▶ transparent-address, block-hash
```

## Steps

| Step                      | Subscriber action                       |
| ------------------------- | --------------------------------------- |
| `Apply { height, data }`  | `data` = one final block: apply it      |
| `Shutdown`                | commit what is buffered, stop           |

`data` on the final stream is a `Final { block, folds }`:

| `folds`       | Where                                     | Writer                                       |
| ------------- | ----------------------------------------- | -------------------------------------------- |
| `None`        | at or below the NFS root (bulk sync)      | folds the block itself onto `Store::staged()` |
| `Some(folds)` | above the root (folded once by the NFS)   | applies `folds.get(kind)` as sent            |

- Once a step is folded, every later one is too, until a restart (the NFS never sends an unfolded
  block over a folded parent). A writer asserts it.
- Heights are contiguous and ascending; a writer asserts no gap above what it holds.

## Start point and restart

The NFS starts the stream after the **lowest** durable tip of every enabled index (its root). An
index ahead of it receives heights it already holds and skips them (`zaino_sync::held`): it still
pops every step, so every queue drains in order. Nothing is sent until every durable tip is the
verified chain's block at its height (`NfsError::Diverged` otherwise: resync).

## Commit cadence: `Committer`

Every writer keeps its store behind a `zaino_sync::Committer`, which decides when to commit:

- the buffer reaches `batch_mib` (bulk sync: one fsync per batch);
- after each run carrying folded steps (the tip: each final block commits as it arrives);
- the stream stays quiet for 1 s.

The idle commit is what lockstep needs: the NFS folds its first block above the root only once
every index holds everything sent, so a writer holding a part-filled batch must commit when the
stream stops. After every commit the writer sends its store's committed view on a `watch`; the NFS
reads it as that index's durable tip and pairs it with its layers in every snapshot.

## Backpressure

One `Arc<T>` per step, N byte-bounded queues. `T: Weight` names what an item holds in memory
(`Final` = block bytes + its `Changes`), and each queued step holds that many bytes of its
subscriber's budget until popped. A step heavier than the whole budget passes alone once the queue
drains. `send` is all or nothing: it holds every queue's share before it pushes to any, so a full
queue delays the step for everyone (what bounds memory and paces the NFS to the slowest index), and
a send cancelled mid-wait delivers nowhere (subscribers never part ways at a stopped publisher's
last step).

`zaino_sink_queue_bytes{sink, subscriber}` is the bytes each queue holds (exact permits, + on
send, − on pop). A gauge at its budget = that index is holding back the NFS.

`Subscription::run(first, budget)` = `first` + every `Apply` already queued behind it, up to
`budget` bytes, never a wait: one run = one fold batch and one compute hop for a writer.

## Indexes publishing to other indexes: fees

Compact-block's records need each transaction's fee, which only value-balance can work out. In
bulk sync value-balance sends one `BlockFees` per **unfolded** step into a `FeeSink`, held heights
included (insert-only: it re-folds them, any later state resolves the same). Compact-block pops one
fee step per unfolded step, skipped ones included, so both queues stay in step whichever index is
ahead after a restart. Folded steps carry nothing on the fee sink: the NFS folded compact-block
after value-balance with the fees already in its `Changes`.

```text
final stream ─┬─▶ value-balance ─▶ FeeSink (unfolded steps only) ─┐
              └─────────────────────────────────────────────────┴─▶ compact-block
```

## Implementation

The sink is about 130 lines (`packages/zaino-sync/src/data_sink.rs`). Each subscriber gets an
unbounded tokio channel paired with a semaphore that holds one permit per byte of its budget.
`send` acquires a step's weight in permits from every subscriber, then pushes to all; each
subscriber's permits go back when it pops the step. `Shutdown` skips the permits, so a full queue never blocks a stop.

Mistakes are loud rather than silent:

- Subscribing needs `&mut` access before the publisher runs: nobody joins mid-stream.
- A subscriber that drops its queue before `Shutdown` panics the sink, and a sink dropped without
  `shutdown()` panics its subscribers. This is also the failure path: a failing writer panics, its
  queues drop, the pipeline stops through these panics. There is no error channel.
- The chain tip is not part of the stream. Serving reads the NFS's snapshots.

[nfs.md](./nfs.md) covers the NFS and the writers;
[`zaino-sync/usage.md`](../../packages/zaino-sync/usage.md) is the API reference.
