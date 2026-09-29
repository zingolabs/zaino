# Sync: one producer, one parse, N indexes

How blocks get from validators into every index. Everything here is plain `async`/`await`: each
stage is one task in zainod's `JoinSet`, and the stages talk through one data structure, the
`BlockSink`.

```
validators (JSON-RPC) ──▶ zaino-source BlockFetchPool ──┐  bulk: ordered, concurrent, decode on every core
                                                        │
zaino-chainview (N validators) ── quorum tip ──▶ ChainHead ──┐  live: hash-linked non-final window, in memory
                                                             │
                                   Producer (owns BlockSink) ◀┘
                                        │ Apply{h, finalized} / Finalized{h} / Reset
                                        ├─▶ IndexFollower ─▶ CompactBlockIndexWriter ◀─────────┐ fees, paired
                                        ├─▶ IndexFollower ─▶ ValueBalanceIndexWriter           │ by height + hash
                                        │                        └─ deliver ─▶ ValueBalanceSink ┘
                                        ├─▶ IndexFollower ─▶ BlockHashIndexWriter
                                        ├─▶ IndexFollower ─▶ TreeStateIndexWriter
                                        └─▶ IndexFollower ─▶ TransparentAddressIndexWriter
```

## One resume point

Each index commits on its own cadence, so after a crash their durable tips differ. The sink
starts at the **rearmost** one and feeds **every** index that same run: an index ahead receives
blocks it already holds and skips them in `IndexWriter::deliver` (the follower sends them
nowhere else, and checks the one at its durable tip is the block it committed).

That is what lets one index feed another without plumbing between them. The value-balance
index resolves each block's fees in `deliver` (bulk, replay and tip alike) and publishes them to
the `ValueBalanceSink`. The compact-block index keeps its lifecycle on the `BlockSink` and, in its
own `deliver`, awaits that block's item (`balances_for`, which skips items a reorg left queued).
If compact-block is behind, value-balance replays the missing heights from its append-only store.
Resolving at commit time instead would deadlock: compact-block waits block by block while a
commit waits for a whole batch.

## The one-way state apply

An index only moves forward from its durable height:

- a **final** block (`height < tip + 1 − finalised_depth`) is staged and fsynced in batches; it can
  never be undone, so it skips pre-commit
- a **non-final** block is folded into pre-commit (in memory, served, never written)
- `Finalized{h}` moves the oldest pre-commit block to the staged batch once the tip buries it
- `Reset` (a reorg) writes what is staged — final is final — and drops pre-commit; the subscriber
  is then fed the winning branch from its first non-final height

`finalised_depth` ≥ `MAX_BLOCK_REORG_HEIGHT` on mainnet and testnet, so no reorg the consensus
permits reaches a final block. A reorg, a restart and a crash therefore all recover the same way:
durable state is reopened at its seals, and everything above it is replayed. Each follower checks every
delivered block's parent against the last one it holds, starting from the stored tip hash, so a
chain that does not link is fatal rather than spliced (`docs/design/durability.md` §5).

## Stages

**`zaino-source` `BlockFetchPool`**: fetches blocks `from..=to` as an ordered stream. Each
height's fetch + decode runs as its own spawned task (decode spreads across cores); at most
`concurrency` are in flight. Heights are spread round-robin across every configured validator
by default; `primary_validator` pins bulk fetch to one.

**`zaino-chainview`**: N validators; quorum tip agreed **by hash** (never max height),
mempool, peer telemetry, broadcast. The producer follows its quorum tip.

**`zaino-chain-head`**: a library, not a task. Holds the canonical non-final window
(`highest tip − finalised_depth ..= tip`) as a hash-linked chain of `Arc<Block>`; its floor is the
sink's final boundary − 1, so every legal fork point is in it. `advance(quorum_tip)` fetches a
clean extension by height (concurrently, through the pool) and otherwise walks back by
`prev_hash` from the quorum tip's hash to the block it links onto. It returns `Unchanged`,
`Extended` or `Reorg { fork }`; the new blocks are read back with `best_chain_from(height)`. A
quorum tip that is a held ancestor of the held tip is a retreat: `Reorg { fork: tip + 1 }` with no
replacement blocks. A fork below the floor is past the consensus reorg bound, and is fatal.

**`zaino-sync` `Producer`**: the only task that touches the `BlockSink`.

1. Bulk: stream `next ..= tip − finalised_depth` from the pool, `add` each (all final), each
   block's `prev_hash` checked against the one before it
1. Anchor the chain head on the last bulk block; the first `advance` fetches the non-final
   window by height
1. Live: on each quorum tip, `chain_head.advance` → `set_tip` + `add` the new blocks; on a reorg,
   `reset`, then `add` the window from the reset's resume height (no fetch). A quorum tip more
   than `finalised_depth` ahead (a long outage) goes back to step 1, but only once the
   validator's block at the held tip's height is the held tip: bulk's `set_tip` finalises the
   whole window. A different block there is stepped onto first, which is an ordinary reorg

**`IndexFollower`**: one per enabled index, drains its `Subscription`, drives its
`IndexWriter`. Ends when the producer drops the sink.

## Invariants (asserted in release builds)

- Every subscriber sees contiguous ascending heights from the rearmost durable extent; one
  started below its own durable tip must replay onto it (`FollowError::Diverged` otherwise)
- Every block added links onto the one before it (bulk checks `prev_hash`; live blocks come out
  of the hash-linked window)
- A reset never reaches a final height (`reset`'s resume height ≤ the fork)
- `Finalized` is emitted oldest first, only for delivered heights
- A final `Apply` arrives only while pre-commit is empty
- Every durable extent only moves forward
- The chain head's window is hash-linked and never spans more than `finalised_depth + 1` heights
