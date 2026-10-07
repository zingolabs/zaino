# The data sink

`IndexerDataSink<T>` is the core data-flow/plumbing type that connects all indexes.
It has a single publisher, and can support an arbitrary number of subscribers.

Subscribers fetch data via a `subscription.next().await` call, on the queue returned by
`BlockSink.subscribe(...)`. For downstream subscribers, it is
guaranteed that calling `next()` in a loop will deliver a fully contiguous stream of block data.

Most of the complexity around scheduling and syncing is consequently offloaded to a
`BlockSink.send(Step::Apply { height, finalized, data }).await` call, and the publisher of this data is responsible
for sending these in contiguous order.

This data-structure is also the primary orchestration point, since each `IndexerDataSink` has a configurable
cache size, and once this cache is full, the `IndexerDataSink.send(...).await` will wait until there is space
in the cache. Once all of the configured subscribers to the `IndexerDataSink` have run `.next().await` on a
block, that block and its data are removed from the cache.

This naturally limits the overall speed of the index construction to the slowest-running subscriber. So if
the transparent-address-index is the slowest, the `BlockSink` will fill up its cache, and the producer will
wait until the transparent-address-index has called `.next().await` and freed up space before writing more
blocks to the cache.

This removes a lot of complexity around monitoring the speed/progress of each index, and allows all of the index
code to run on an async runtime, and be quite simple.

```text
 header chain (VerifiedChain) ──┐
                                ├─▶ Producer ──▶ BlockSink ─┬─▶ [≤ 256 MiB] ─▶ compact-block
 any source (getblock <hash>) ──┘   (checked)               ├─▶ [≤ 256 MiB] ─▶ tree-state
                                                            └─▶ [≤ 256 MiB] ─▶ transparent-address
```

## Steps

There are 4 pieces of data that can be written into the sink. Apply will be the primary data, and is the contiguous stream
of block-num -> {Data: <T>, Finalized: bool}. If finalized is true, the downstream index can and should update the index, and persist
the data to durable storage. If finalized is false, the index can update its in-memory state, but not persist the data.

The Finalized step signals to the indexer that a previously applied block is now finalized. This will occur every time a new block is mined,
and the producer will track the chain tip, and then emit a Finalized(tip_block-1000) followed by an Apply(tip_block, Data, False)

The Reorg step signals to downstream indexers that the best-chain has moved, and each indexer should flush its finalized state to disk, then
discard the non-finalized in-memory state. Next, the Producer will publish 1000 Apply() operations that represent the new best-chain

We currently design in this inefficiency, replaying all 1000 non-finalized blocks instead of just the 10 changed blocks to limit the complexity
of the downstream indexing logic. With this design, each downstream index only needs a one-direction state transition function, and a notion of
"finalized" state. If this does become an issue down the road, we can engineer in some non-finalized snapshots that can be rolled back to,
but this design of just rolling back completely to the finalized-state, and then replaying all 1k non-finalized blocks is the simplest and least
bug prone

Since the pipeline flows from the validator-rpc producer, down to all the indexers, we also add a Shutdown step that will trigger indexers to
flush their finalized state to disk, and then stop.

| Step                                | Subscriber action                                                             |
| ----------------------------------- | ----------------------------------------------------------------------------- |
| `Apply { height, finalized, data }` | final: stage for disk. Otherwise: apply to the non-finalized state            |
| `Finalized { height }`              | move `height` from the non-finalized state to disk                            |
| `Reorg`                             | flush what is final, drop the non-finalized state; the winning branch follows |
| `Shutdown`                          | flush what is final, stop                                                     |

## A reorg, as one subscriber sees it

Durable to 1233; 1236 is replaced by 1236′.

| Step               | Final  | Non-finalized |
| ------------------ | ------ | ------------- |
| `Apply 1234 final` | ..1234 |               |
| `Apply 1235`       | ..1234 | 1235          |
| `Apply 1236`       | ..1234 | 1235 1236     |
| `Finalized 1235`   | ..1235 | 1236          |
| `Reorg`            | ..1235 |               |
| `Apply 1236′`      | ..1235 | 1236′         |
| `Apply 1237`       | ..1235 | 1236′ 1237    |

The replay starts at the first non-final height; final blocks are never re-sent. `Reorg` carries no
fork height because every legal fork is above the final boundary.

## How the producer publishes

The producer follows the header chain's `VerifiedChain` and nothing else
([verified-chain.md §9](./verified-chain.md#9-the-producer)). It holds every block it has sent
above the final tip, each with its hash: that window is how it sees a reorg, and what it replays
from. On each `VerifiedChain` it reads (the latest one: chains it never saw produce no steps), in
this order:

1. **Finality.** A `Finalized` for each window block, oldest first, that is at or below the
   chain's final tip **and** still the chain's block at its height. Those leave the window.
1. **Reorg.** The fork is the first window block whose hash is not the chain's at its height (a
   height above the new best counts: a retreat). If there is one: `Reorg`, then the window's
   blocks below the fork are sent again from memory (no fetch), then the new branch.
1. **Delivery.** Each next height's block, once fetched from any source and checked against the
   chain's header (hash, coinbase height, merkle root rebuilt from its txids), as an `Apply` whose
   `finalized` says whether it is at or below the final tip.

What a subscriber can rely on, whatever the chain does:

- **Final never rolls back.** A `Reorg` only drops non-final state; the header chain's final tip
  never moves back and a final block never changes (the producer asserts both).
- **Every `Apply` is the verified chain's block at its height, body included**, for the chain it
  was sent under. A source that served another block or a doctored body is passed over and the
  block is asked of another source; it never reaches a subscriber.
- **One finality.** `finalized` and `Finalized` follow the header chain's final tip alone. A
  `Reorg` and finality moving across the fork can arrive in one chain update: the window's blocks
  below the fork that are final go out as `Finalized` first, then `Reorg`, then the new branch's
  blocks at heights now final arrive as final `Apply`s.
- **A retreat is a reorg.** A heavier, shorter branch sends `Reorg` and replays up to the new best;
  heights above it come back only once the chain grows again.
- **Restart.** Nothing is sent until every subscriber's durable tip is at or below the final tip
  and is the chain's block there; one that is not stops the producer (`ProduceError::Diverged`).

A reorg that also finalizes across the fork. Final through 1233, A1234..A1237 sent non-final; the
next chain read has B (forking after A1234) as best at 1239, final through 1236:

| Step                | Why                                                    |
| ------------------- | ------------------------------------------------------ |
| `Finalized 1234`    | A1234 is still the chain's block and now final         |
| `Reorg`             | A1235 is not the chain's block at 1235: the fork       |
| `Apply B1235 final` | the final tip reached 1235 in the same update          |
| `Apply B1236 final` |                                                        |
| `Apply B1237`       | above the final tip: non-final from here               |
| `Apply B1238`       |                                                        |
| `Apply B1239`       | the best; the index's serving gate opens on this block |

## Start point

The stream starts after the **rearmost** durable tip.
An indexer that subscribes to this data can choose to skip over stuff it has already persisted within its
internal logic, but it MUST run `.next().await` contiguously to signal to the producer that all subscribers have
consumed the data stream

## Indexes publishing to other indexes

An index can also be a publisher. Some indexes need per-block data that another index computes,
and rather than having them query each other, the upstream index republishes every step it
follows into a second `IndexerDataSink` of its own, one for one, with its computed item in place
of the block in each `Apply`. In the code, the upstream index's own loop sends those steps, and
the consumer's loop awaits one step off each queue: its block step, then the derived one.

The compact-block index is the example today. Each compact block needs its fees, and only the
value-balance index can work those out, because it tracks every output's value. So value-balance
publishes a `FeeSink`, and compact-block awaits one fee step after each block step.

```text
BlockSink ─┬─▶ value-balance ─▶ FeeSink ─┐
           └─────────────────────────────┴─▶ compact-block (block, then its fees)
```

Since both streams carry exactly the same steps, reorgs, replays and shutdown line up without any
extra code. A new derived stream is one more sink an index's loop sends into.

## Implementation

The sink is about 130 lines (`packages/zaino-sync/src/data_sink.rs`). Each subscriber gets an
unbounded tokio channel paired with a semaphore that holds one permit per byte of its budget.
`send` acquires a step's weight in permits from each subscriber in turn, and the permits go back
when that subscriber pops the step. `Shutdown` skips the permits entirely, so a full queue can
never block a stop.

A few mistakes are made impossible or loud rather than silent:

- Subscribing needs `&mut` access and the producer takes the sink by value, so nobody can
  subscribe once data is flowing.
- A subscriber that drops its queue before `Shutdown` panics the sink, and a sink dropped without
  `shutdown()` panics its subscribers. Either way the process stops instead of quietly losing
  blocks. This is also the failure path: a failing index panics, its queues drop, and the
  pipeline stops through these two panics. There is no error channel.
- The chain tip is deliberately not part of the stream. Each index's serving gate reads the
  header chain's `VerifiedChain` directly, which keeps the sink purely about blocks.

[sync.md](./sync.md) covers the producer and the index loops, and
[`zaino-sync/usage.md`](../../packages/zaino-sync/usage.md) is the API reference.
