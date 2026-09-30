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
 zaino-source (bulk) ───────────┐
                                ├─▶ Producer ──▶ BlockSink ─┬─▶ [≤ 256 MiB] ─▶ compact-block
 zaino-non-finalized-state ─────┘                           ├─▶ [≤ 256 MiB] ─▶ tree-state
 (tip, reorgs)                                              └─▶ [≤ 256 MiB] ─▶ transparent-address
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

## Start point

The stream starts after the **rearmost** durable tip.
An indexer that subscribes to this data can choose to skip over stuff it has already persisted within its
internal logic, but it MUST run `.next().await` contiguously to signal to the producer that all subscribers have
consumed the data stream

## Indexes publishing to other indexes

An index can also be a publisher. Some indexes need per-block data that another index computes,
and rather than having them query each other, the upstream index republishes every step it
follows into a second `IndexerDataSink` of its own, one for one, with its computed item in place
of the block in each `Apply`. In the code, an index that does this implements the `Derives` trait
(its `derive(&blocks)` returns one item per block), and the consumer receives each block together
with that item as `Paired { upstream, derived }`.

The compact-block index is the example today. Each compact block needs its fees, and only the
value-balance index can work those out, because it tracks every output's value. So value-balance
publishes a `FeeSink`, and compact-block reads it alongside its `BlockSink` queue through `Zip`,
which pops one step from each and asserts they are the same step.

```text
BlockSink ─┬─▶ value-balance ─▶ FeeSink ─┐
           └────────────────────────────▶ Zip ─▶ compact-block (block + fees)
```

Since both streams carry exactly the same steps, reorgs, replays and shutdown line up without any
extra code. A new derived stream is an `IndexWriter` that also implements `Derives`, wired up with
`.publishing(sink)` on its follower.

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
  blocks.
- The chain tip is deliberately not part of the stream. Indexes read it from `zaino-chainview`
  directly, which keeps the sink purely about blocks.

[sync.md](./sync.md) covers the producer and followers, and
[`zaino-sync/usage.md`](../../packages/zaino-sync/usage.md) is the API reference.
