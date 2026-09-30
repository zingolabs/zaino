# Sync

Sync is how blocks get from the validators into every index. There is one producer, each block is
decoded once, and any number of indexes consume it. Every stage runs as a single task in zainod's
`JoinSet`, and the stages only meet at the `BlockSink` ([data-sink.md](./data-sink.md) covers the
steps, a reorg walkthrough and backpressure).

```text
validators ──▶ BlockFetchPool (bulk: ordered, concurrent, decoded on every core) ──┐
chainview (quorum tip) ──▶ ChainHead (non-final window, hash-linked) ───────────────┤
                                                                                   ▼
                                                              Producer ──▶ BlockSink
                                                                            ├─▶ compact-block ◀─ Zip ─ FeeSink ◀─┐
                                                                            ├─▶ value-balance ───────────────────┘
                                                                            ├─▶ block-hash
                                                                            ├─▶ tree-state
                                                                            └─▶ transparent-address
```

## Stages

The `BlockFetchPool` in `zaino-source` turns a height range into an ordered stream of decoded
blocks. Each height is fetched and decoded in its own task, with at most `concurrency` in flight,
and heights are spread round-robin across the configured validators. Setting `primary_validator`
pins bulk fetching to one of them.

`zaino-chainview` provides the quorum tip, which the validators must agree on by hash rather than
by highest height. The producer follows that tip, and every follower also reads it directly to
decide when its index is serving and when to switch from batched commits to one commit per block.
Blocks are only ever fetched from the validators listed in the tip's `agreed_by`, because a
validator that disagrees may still be serving a stale branch at a height the tip has already made
final.

The `ChainHead` in `zaino-non-finalized-state` is a library rather than a task. It holds the
non-final window as a hash-linked chain of `Arc<Block>`, and its floor sits one below the final
boundary so every legal fork point is inside it. On each new tip, `advance(tip)` either extends
the window by height, or walks back along `prev_hash` to find the fork, and reports `Unchanged`,
`Extended` or `Reorg { fork }`. A tip that retreats onto one of our own ancestors is also reported
as a reorg, with no replacement blocks. A fork below the floor is outside consensus and is fatal.

The `Producer` is the only thing that writes to the `BlockSink`. It bulk-syncs from its start
height up to `tip − finalised_depth`, sending every block as final and checking each `prev_hash`
against the block before it. The end of that range moves up as the tip moves, so one bulk pass
covers the whole catch-up. It then anchors the chain head on the last bulk block and follows the
tip: each `advance` becomes a `Finalized` for every height the tip has buried, plus an `Apply` for
each new block. On a reorg it sends `Reorg` and replays the winning branch from the first
non-final height, straight out of the chain head's window with no fetching. If the tip ever gets
more than `finalised_depth` ahead, for example after a long validator outage, the producer goes
back to bulk sync, but only once it has confirmed the validator still holds our tip. Otherwise it
steps onto the validator's block first, which is just an ordinary reorg.

Each index gets one `IndexFollower`, which drains its queue into the index's `IndexWriter` until
`Shutdown`. A follower that fails cancels the whole pipeline, but it keeps draining its queue
through `Shutdown` so the sink never sees a dropped queue.

## Fees: an index publishing to another index

The compact-block index needs each block's fees, and only the value-balance index can work those
out. We connect the two with a second sink: value-balance republishes its steps into the
`FeeSink`, and compact-block reads that alongside its `BlockSink` queue through `Zip`. Since both
streams start from the same rearmost height, a compact-block index that is behind still gets its
fees, because value-balance replays them from its own store.

We deliberately do not look fees up at commit time instead. Compact-block waits for fees block by
block while a commit waits for a whole batch, so the two would deadlock.

## Invariants

These are asserted in release builds, and a violation stops zainod:

- Every index sees contiguous heights from after the rearmost durable tip, and a replay that
  starts below an index's own durable tip MUST land exactly on it (`FollowError::Diverged`).
- Every block MUST link onto the one before it, starting from the stored tip hash
  (`FollowError::Unlinked`, see [durability.md](./durability.md) §5).
- `finalised_depth` is at least `MAX_BLOCK_REORG_HEIGHT` on mainnet and testnet, so no replay can
  reach a final height.
- `Finalized` is sent oldest first, and only for heights already delivered.
- A final `Apply` only arrives while the index's non-finalized state is empty.
- Durable tips only ever move forward.
- The chain head never spans more than `finalised_depth + 1` heights.
