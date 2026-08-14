# `zaino-chain-head` — usage

The domain half of the chain head subsystem: the vocabulary and ports for the
bounded, non-finalised head of the chain. No runtime, no data structures. The
runtime is [`zaino-chain-head-service`](../zaino-chain-head-service/usage.md).

Depend on this crate to *name* what the chain head answers. Depend on the
service crate only if you are the one starting it.

```rust
use zaino_chain_head::{ChainHeadBlockService, ChainHeadSnapshot};

/// Works against any chain head runtime, and cannot start or stop one.
fn tip_height<H: ChainHeadBlockService>(head: &H) -> u32 {
    head.current().best_tip().height
}
```

## Ask the snapshot, not the handle

The handle has two methods — `current()` and `subscribe_updates()` — and that is
the whole of it. Everything answerable *about the chain* is on
`ChainHeadSnapshot`.

This is not an arbitrary split. Capture one snapshot and ask it several
questions and the answers are consistent with each other, because they describe
one instant. Ask the handle repeatedly and each answer comes from whatever was
published at that moment, so a block height and the block at that height can
disagree.

```rust
// Right: one view, consistent answers.
let snapshot = head.current();
let tip = snapshot.best_tip();
let block = snapshot.get_block_by_height(tip.height);

// Wrong: two views, and the chain may have moved between them.
let tip = head.current().best_tip();
let block = head.current().get_block_by_height(tip.height);
```

So **do not add query methods to `ChainHeadBlockService`**. Every one defines a
capability twice and offers a caller a way to accidentally straddle two views.

A snapshot is immutable and independent of the runtime that made it. Holding one
across a reorg is fine and is the intended way to serve a long request: it keeps
describing the chain as it was, and the reorg is visible when you next take one.

### Naming which chain state you are on

`ChainStateEpoch` is the name of a chain state: a generation that advances when
the canonical tip changes — not on every republication — plus the tip itself.
It is readable from both sides, and which one you want is not a matter of taste:

- `snapshot.epoch()` — the epoch of the view you are holding.
- `head.subscribe_updates()` — a feed of epochs as they are published.

Use the snapshot's when deciding whether some other component's data is coherent
with the chain your caller is being served. Reading the handle's epoch for that
compares against whatever has been published since, which is exactly the race
the epoch exists to close. Use the feed only to *wake* on a change, then re-read
what you actually need.

```rust
// Right: the set is coherent with the view this caller is reading against.
let snapshot = head.current();
if other.is_valid_for(snapshot.epoch()) { /* serve from `snapshot` */ }

// Wrong: the handle may have moved past the snapshot the caller holds.
if other.is_valid_for(head.current().epoch()) { /* serve from `snapshot` */ }
```

## There is no way to make it synchronise

No `sync`, no `sync_to_height`, no `reconcile`, no `advance` — not on any port,
at any visibility. The chain head owns a writer task and keeps itself current.

This is load-bearing rather than stylistic. A consumer able to drive the chain
head can sequence it against something else, and that is exactly the coupling
the subsystem was extracted to remove: the original was stepped by the chain
index's sync worker, so tip freshness was decided by database write throughput.

If a test needs deterministic stepping, the service crate has a path for it that
is compiled out of production builds. Do not add one here.

Lifecycle is absent for the same reason. Starting, stopping and status are
inherent methods on the concrete service; a read handle cannot shut the chain
head down because there is no method on it that could.

## Work is anchor-relative

`ChainHeadWork` is accumulated from the chain head's **own anchor**, not from
genesis. It orders competing branches correctly, which is all the chain head
needs, and it is not the absolute chainwork a validator reports.

The distinct type is there to stop the two being confused. Do not serve a
`ChainHeadWork` where an API promises chainwork, and do not compare one against
a value from a validator — two chain heads with different anchors produce
different numbers for the same block.

## The driven port names only what is asked

`ChainHeadBlockSource` is a bound alias over five `zaino-source` ports with a
blanket impl. Nothing implements it directly: a type answering all five earns
the bound automatically, so production composites and test mocks qualify the
same way.

Add a port to that bound **only when the chain head actually asks the question**.
The bound is a statement of requirements, and every entry obliges every source
to answer. `GetChainTips` was in it and was removed for precisely this reason —
the chain head learns of a competing branch by living through the reorg that
created it, never by asking a validator to enumerate tips. See ADR-0011, which
also records that `zebra-rpc` does not implement `getchaintips` at all.

The bound is deliberately not `Clone`. A source may own connections and a
database handle that must not be duplicated; the runtime shares one behind an
`Arc`.

## What the chain head is not

It is not an index. It holds a bounded window and nothing below it, never reads
the finalised state, and never scans history. Complete answers — an address's
whole balance, a transaction's full status — combine the chain head with the
finalised state and the mempool, and that combining belongs to the consumer.

A retained block is a parsed projection, not the consensus bytes, so raw
transaction and raw block queries cannot be served from here.

## Freeze events are best-effort

`ChainHeadFreezeEvents` carries blocks that have fallen below the consensus seam,
whole and with their tree roots, so a chain store can ingest without re-fetching.

Treat gaps as normal. It is a broadcast channel — the chain head follows the tip
and will not stall on a slow consumer — so a lagging subscriber gets
`RecvError::Lagged(n)`, and a chain head re-anchoring after an outage never
emits what it skipped. A store must be able to build from source regardless;
this only spares it the fetch in steady state. Building a consumer that assumes
contiguity is the mistake this section exists to prevent.
