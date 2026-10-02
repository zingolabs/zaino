# The finality seam: one owner for the FS/NFS boundary

Status: designed, not started. Scope: new crate `zaino-finality`, consumed by
`zaino-indexer` and `zaino-chain-head-service`, constructed by `zaino-runtime`.
Base: `refactor/store-service-split`.

## Problem

The boundary between the finalised tier and the volatile one is derived twice,
from two constants, over two tips, and nothing in the type system objects.

The finalised side derives its ceiling from its own view of the chain:

```rust
// zaino-indexer/src/source_provisioner.rs
fn finalised(&self, tip: Height) -> Height {
    tip.saturating_sub(self.finalised_depth)      // config.indexer.finalised_depth
}
```

The volatile side derives its floor from its own tip and a second constant:

```rust
// zaino-chain-head-service/src/service.rs
let reorg_safety_floor = height_below(graph.best_tip().height, self.config.max_depth());
let confirmation_floor = match *self.confirmed_watermark.borrow() { .. };
graph.remove_finalised_blocks(reorg_safety_floor.min(confirmation_floor));
```

One leg of the handshake is wired: the indexer publishes its committed
watermark and the chain head consumes it to gate trimming
(`boot.rs`, `driver.subscribe_confirmed_watermark()` into
`ChainHeadService::anchor`). The other leg is not, and the code says so:

> **`finalised_depth` is the *standalone* seam derivation** (`tip − depth`, for
> an indexer with no chain-head). In the composed runtime the seam has a single
> owner — the chain-head's floor — which the indexer must *consume*, not
> re-derive, so FS-ceiling and NFS-floor cannot drift. That path replaces this
> depth with the chain-head's published seam when the chain-head is wired in.

So the composed runtime runs the standalone derivation. `config.indexer.finalised_depth`
is operator-settable while the chain head's `max_depth` is `MAX_BLOCK_REORG_HEIGHT`,
and setting the former below the latter makes the finalised store commit heights
the volatile tier still treats as reorg-able. There is no rewind path anywhere in
`zaino-store`, `zaino-sync`, `zaino-indexer`, or `zaino-persistence`
(`WatermarkRepair` corrects an over-claiming stamp at boot; it is not a runtime
rewind), so that fault is unrecoverable and silent.

`RETENTION_MARGIN = 10` absorbs a tick of skew between two parties reading the
boundary at slightly different instants. It does not absorb a configuration
mismatch.

## The model

Three heights, all monotone non-decreasing, where `t` is the validator's tip and
`d` the consensus reorg depth:

```
r      the reorg horizon      — highest height past the reorg window
w      the durable watermark  — highest height committed to disk
floor  the retention floor    — lowest height the volatile tier retains
```

The whole contract is one chain of inequalities:

```
genesis  ≤  floor + margin  ≤  w  ≤  r  ≤  t − d  ≤  t
```

Each link has exactly one party permitted to move its left-hand side. That is
the single-owner rule stated precisely rather than as a comment:

| quantity | owned by      | bounded above by | source of the bound |
|----------|---------------|------------------|---------------------|
| `r`      | volatile tier | `t − d`          | its own graph       |
| `w`      | durable tier  | `r`              | read across the seam |
| `floor`  | volatile tier | `w − margin`     | read across the seam |

The seam therefore carries exactly two quantities, `r` and `w`. `floor` is
internal to the volatile tier and never crosses it. Two quantities, two halves,
two messages.

Gapless served coverage is a consequence of link 1, not a separate rule:

```
C_durable  = [genesis, w]
C_volatile = [floor, t]

C_durable ∪ C_volatile = [genesis, t]    ⟸    floor ≤ w
```

## What `r` is, and what it is not

```
r = height(t) − d          where t is the volatile tier's canonical tip
```

`r` depends only on `t`, which is external to the seam. Nothing in its
definition refers to retention, so the ratchet has no cycle.

The volatile tier owns `r` not because it computes a different number than the
indexer would, but because of what backs the number:

1. **`t` is a verified tip** — the tip the volatile tier has linked into a
   gapless canonical chain, not a height from an independent RPC read that may
   lead or lag what that tier has actually processed. This is the coherence the
   single-owner rule buys.
2. **`r` carries a hash backed by parent links.** `ChainGraph` guarantees that
   *canonical heights run without gaps up to the tip* and that *each canonical
   block's parent is the canonical block one height below*, so the hash at `r`
   is verified by an unbroken parent chain down from `t`. A height from a
   formula has nothing behind it.

The hash is the entire graph-derived content of the signal, which makes
`BranchMismatch` the load-bearing fault rather than a diagnostic.

**Rejected: conditioning `r` on the absence of retained competing branches at
that height.** It adds no safety. A competing block heavy enough to threaten the
canonical block at `k` would already have moved the canonical chain through the
graph's own tip selection, so condition 2 covers it; one not heavy enough would
need a reorg deeper than `d` from `t`, which the reorg bound excludes. The
condition would only observe that the volatile tier has not yet swept branches
it no longer needs — bookkeeping of no interest to the durable tier. It is also
not merely redundant but harmful: it makes `r` depend on retention, retention
depend on `floor`, and `floor` depend on `w ≤ r`, closing a cycle in which an
abandoned branch below `t − d` pins the whole ratchet and is never trimmed
because the ratchet is pinned. Keeping `r` free of the graph's retention state
is what keeps the model acyclic.

## Separation of concerns against `ChainGraph`

`ChainGraph`'s invariants and the seam's differ on two axes, which is why they
stay separate and why they need different test strategies:

|                   | `ChainGraph`                             | the seam                          |
|-------------------|------------------------------------------|-----------------------------------|
| quantifies over   | blocks retained in one graph             | boundary heights across two tiers |
| modality          | **state** — holds after every move       | **trace** — holds across successive states |

Monotonicity is a property of successive states, so the seam's invariants are
verified over sequences, not per-move. The entire contact surface between the
two is one read: the graph's tip height and the hash `d` below it. `ChainGraph`
is unchanged by this work.

## The crate

`zaino-finality` holds the contract and nothing else. It is not
`zaino-consensus`: `MAX_BLOCK_REORG_HEIGHT` is a consensus fact, but *where this
deployment's durable tier ends* is a composition fact derived from it.

```rust
pub struct Seam { /* .. */ }

impl Seam {
    pub fn new(reorg_depth: u32, retention_margin: u32) -> Self;
    pub fn split(self) -> (ReorgHorizon, DurableWatermark);
}

/// Held by the volatile tier. Owns `r`, reads `w`.
pub struct ReorgHorizon { /* .. */ }          // not Clone
impl ReorgHorizon {
    /// `d`, so the caller knows which height's hash to supply.
    pub fn reorg_depth(&self) -> u32;
    /// Publishes `r = tip − d`, computed here: the caller supplies its verified
    /// tip and the hash of the canonical block `d` below it, never `r` itself.
    pub fn advance(&mut self, tip: Height, hash_at_horizon: BlockHash)
        -> Result<Released, SeamFault>;
    pub fn durable(&self) -> Option<Committed>;
    pub fn retention_floor(&self) -> Option<Height>;    // w − margin, computed in one place
}

/// Held by the durable tier. Owns `w`, reads `r`.
pub struct DurableWatermark { /* .. */ }      // not Clone
impl DurableWatermark {
    pub fn released(&self) -> Option<Released>;
    pub fn await_released(&mut self) -> impl Future<Output = Released> + Send;
    pub fn advance(&mut self, authorised_by: &Released,
                   to: Height, hash: BlockHash) -> Result<Committed, SeamFault>;
}

pub struct Released  { /* private */ }   // r, issued only by ReorgHorizon::advance
pub struct Committed { /* private */ }   // w, issued only by DurableWatermark::advance
```

`advance` is the same verb on both halves because it is the same operation: move
the quantity you own. Reading the other tier's quantity is a getter named for
that quantity.

Their arguments differ in kind, and the asymmetry is the model's: `w` is
whatever the durable tier actually committed, so it names it directly, while `r`
is a function of `t`, so the volatile tier supplies `t` and the seam applies the
function.

### What the types enforce

- **A component cannot publish the fact it does not own.** Each half exposes
  `advance` for one quantity and a getter for the other.
- **A component cannot duplicate its half.** Neither half is `Clone`, and
  `advance` takes `&mut self`, so single-writer holds at the borrow checker as
  well as at the type level.
- **A component cannot forge an authorisation.** `Released` has private fields
  and no public constructor; the only code that builds one is
  `ReorgHorizon::advance`. `DurableWatermark::advance` requires one, so *"the
  durable tier advanced on its own authority"* is unrepresentable rather than
  merely checked.
- **A component cannot name a horizon inside its own reorg window.** `d` lives
  in the seam and `r` is derived there, so the volatile tier supplies a tip and
  never an `r`.
- **Boot cannot forget a leg.** Both halves are constructor arguments, so a
  component that is not wired does not compile.

`Seam::new` is public, so constructing a second seam is possible. That is the
one remaining miswiring, it lives in one function, and it fails loudly within
seconds — two halves from two seams means the volatile tier never trims and the
durable tier never advances, both reported. It is covered by a boot test rather
than by contorting the API. The trade accepted here is deliberate: an
unforgeable token and an unreachable constructor cannot both be had across a
crate boundary in safe Rust, and forgery is the silent fault while a second seam
is the loud one.

### Faults, one per violated link

| variant                 | condition          | meaning |
|-------------------------|--------------------|---------|
| `RegressedHorizon`      | `r' < r`           | a reorg deeper than `d`; consensus failure |
| `RegressedWatermark`    | `w' < w`           | no rewind path exists, so always corruption |
| `WatermarkPastHorizon`  | `w' > r`           | volatile heights written into an append-only store |
| `BranchMismatch`        | `w' = r ∧ hash(w') ≠ hash(r)` | the tiers are on different branches at the seam |

`WatermarkPastHorizon` is the fault the current configuration can produce and
nothing catches.

`r ≤ t − d` needs no fault variant. The seam owns `d` and derives `r` from the
tip the caller supplies, so a horizon inside the reorg window is not a rejected
value but an unrepresentable one — the caller never names `r`.

The hash check in `BranchMismatch` bites only when the durable tier commits at
exactly `r`, since the seam retains one `Released` rather than a history.
During bulk sync the durable tier lags by millions of heights and commits far
below `r`, where branch disagreement is not a real scenario; near the tip the
two coincide and the check applies. Retaining a history to close the gap would
be unbounded during bulk sync.

## What this deletes

- `config.indexer.finalised_depth` from the composed path. It survives on a
  separate standalone constructor for the isolated-store and bench cases, where
  deriving the boundary locally is honest.
- The `watch::Receiver<Option<Height>>` threaded through `boot.rs` and
  `ChainHeadService::anchor`, replaced by the half.
- `RETENTION_MARGIN` as a constant private to the chain-head service; it becomes
  a `Seam::new` parameter, read in one place.

## Testing

The seam's invariants are trace properties, so the unit tests drive sequences of
`advance` calls against a `Seam` and assert the inequality chain holds after
each step, including each fault's rejection leaving the state unchanged.

Components become generic over their half, which admits a `StubSeam` in-crate
behind the `testing` feature. The chain head's trim logic and the indexer's
advance logic are then unit-testable in isolation, which neither is today.

`zaino-core/examples/seam_run.rs` is promoted to the integration test of the
real ratchet. It currently has to disable both legs to stay deterministic — a
watermark channel that is never published to, and `finalised_depth: 0` — so
wiring a real `Seam` through it exercises the handshake end to end over the
offline `MockChain`.

## Noted, not addressed

During initial sync the confirmation floor holds the volatile window open for
the whole catch-up, since `w` lags far behind `t`. At measured sync times this
is a few thousand extra retained blocks, not a leak, but it is a consequence of
link 1 worth recording so it is not rediscovered as one.
