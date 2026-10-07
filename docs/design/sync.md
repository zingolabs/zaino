# Sync

Sync is how blocks get from the validators into every index. There is one producer, each block is
decoded once, and any number of indexes consume it. Every stage runs as a single task in zainod's
`JoinSet`, and the stages only meet at the `BlockSink` ([data-sink.md](./data-sink.md) covers the
steps, a reorg walkthrough and backpressure).

```text
header sync ── watch<VerifiedChain> ─────────────────────────────┐
validators ── getblock <hash> 0 (any, each block checked) ───────┤
                                                                 ▼
                                                              Producer ──▶ BlockSink
                                                                            ├─▶ compact-block ◀───── fees ─ FeeSink ◀─┐
                                                                            ├─▶ value-balance ────────────────────────┘
                                                                            ├─▶ block-hash
                                                                            ├─▶ tree-state
                                                                            └─▶ transparent-address
```

## Stages

Header sync publishes the `VerifiedChain`: Zaino's own header chain, every header checked against
the consensus rules including proof of work, its best tip the most-work one, its final tip
`finalised_depth` below once a trusted validator holds it
([verified-chain.md §3–§4](./verified-chain.md#3-the-shape)). It names the hash and merkle root
of every block up to the best, so the producer needs nothing else to know what to fetch and
whether what came back is right.

The `Producer` is the only thing that writes to the `BlockSink`, and it follows that chain alone
([verified-chain.md §9](./verified-chain.md#9-the-producer)). It is a pure `ProducerCore` (the
verified chain + what the sink holds → the steps to send and the blocks to fetch; no I/O, time an
input) inside a thin async driver that runs the fetches and the sends:

- **Fetch from anyone, check everything.** Each wanted `(height, hash)` goes to the least-loaded
  source as `getblock <hash> 0` on its own task (decode and check spread across the runtime). A
  block is accepted only if its hash is the chain's, its coinbase height is the height asked, and
  the merkle root rebuilt from its txids is the header's. Anything else is that source
  misanswering (never an invalid header: ZIP 256 lets a valid header travel with a doctored v5
  body); the source is skipped for a minute and the block asked of another. A source silent past
  15 s is hedged with another; a block every source failed is asked again after a second.
- **Pipelined.** Up to `concurrency` blocks are in flight ahead of the next one sent; they arrive
  in any order and are sent in height order.
- **One finality.** A block is sent final when it is at or below the header chain's final tip;
  each non-final block it holds gets its `Finalized` once the final tip passes it. The producer
  keeps no tip of its own.
- **Reorgs are a hash comparison.** The first held non-final block that is no longer the chain's
  block at its height is the fork: `Reorg`, then the still-best blocks below it again from memory,
  then the new branch. The exact contract subscribers see is in
  [data-sink.md](./data-sink.md#how-the-producer-publishes).
- **Restart.** Production starts after the rearmost durable tip, and nothing is sent until the
  final tip covers every durable tip and each is the chain's block there. One that is not is
  proof the index holds another chain: `ProduceError::Diverged`, and zainod stops.

Each index's serving gate (a small task beside its loop) reads the same `VerifiedChain`: it opens
once the index's applied block **is** the best block (hash, not height), and closes when that
block leaves the best chain, falls more than `finalised_depth` behind it, or a reorg is replaying.

Each index runs its own loop over its queue until `Shutdown`, one `match` arm per step
([`zaino-sync` usage](../../packages/zaino-sync/usage.md#an-index-loop)). In bulk, final blocks
stage until a byte batch fills, so one write is one fsync of a steady size; at the tip every
`Finalized` is written at once, so the durable tip trails the chain tip by exactly
`finalised_depth`.

An index loop cannot fail with an error, because its `run` returns `()`. Anything it cannot
recover from panics where it happens: a failed commit (naming the index and its directory, and
whether the disk is full), chain data it cannot take, or a broken invariant. zainod aborts on the
first panic, and every index reopens at its last durable manifest on restart. Nothing drains and
nothing cancels; see [Failure](../../packages/zaino-sync/usage.md#failure-panic-never-err).

## Fees: an index publishing to another index

The compact-block index needs each block's fees, and only the value-balance index can work those
out. We connect the two with a second sink: value-balance republishes its steps into the
`FeeSink`, and compact-block awaits one fee step after each block step off its `BlockSink` queue.
Since both
streams start from the same rearmost height, a compact-block index that is behind still gets its
fees, because value-balance replays them from its own store.

We deliberately do not look fees up at commit time instead. Compact-block waits for fees block by
block while a commit waits for a whole batch, so the two would deadlock.

## Invariants

The producer's are P1–P6 ([verified-chain.md §10](./verified-chain.md#invariants)): asserted by
`ProducerCore::check` after every step in tests and debug builds, its preconditions asserted in
every build, each seen firing on a planted bug (`producer/core/fire_drills.rs`), and driven by a
model against a naive index (`producer/core/model.rs`). In release builds a violation stops
zainod:

- Every index sees contiguous heights from after the rearmost durable tip, and every durable tip
  MUST be the final verified chain's block at its height (`ProduceError::Diverged`). The producer
  checks it; indexes trust the stream.
- Every `Apply` is the verified chain's block at its height, body included.
- The final tip never moves back and a final block never changes, so no replay reaches a final
  height.
- `Finalized` is sent oldest first, and only for heights already delivered.
- A final `Apply` only arrives while the index's non-finalized state is empty.
- Durable tips only ever move forward.
