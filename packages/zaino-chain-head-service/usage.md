# `zaino-chain-head-service` — usage

The chain head runtime: one writer task that keeps a bounded window below the
tip reconciled with a validator, publishing it as immutable snapshots.

The ports and vocabulary are in
[`zaino-chain-head`](../zaino-chain-head/usage.md). Depend on this crate only if
you start a chain head; everything that merely reads one should bound on
`ChainHeadBlockService` and never name `ChainHeadService` at all.

```rust
use std::sync::Arc;
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_head_service::ChainHeadService;

let head = ChainHeadService::spawn(
    Arc::new(validator),
    ChainHeadConfig::default(),
    cancel_token.child_token(),
)
.await?;

let reader = head.subscriber(); // hand this out, not `head`
```

Hand out the subscriber. It produces snapshots and reports status, and can do
nothing else — no starting, no stopping, no stepping. Handing out the service
instead makes that a convention rather than a property of the types.

Status is on the read handle because reading it is not driving: a snapshot looks
identical whether the writer is keeping up or has given up, so a consumer holding
only the subscriber would otherwise have no way to tell whether the tip it is
serving is fresh. It reads the service's own cell, so the two handles cannot
disagree.

## Startup is fallible, and steady state is not

`spawn` anchors a complete window before it returns, or fails.

A chain head has no persistent state and no second data source, so one that
cannot reach its validator has nothing whatsoever to offer. Failing here rather
than existing in a degraded state is what makes `current()` total for the rest of
the process's life — there is no "not ready yet" case for any read path to
handle.

Once running it does not fail. A validator that becomes unreachable leaves the
last published snapshot in place and moves the status to `RecoverableError`,
then to `CriticalError` once `max_consecutive_failures` is spent. Stale data with
a status saying it is stale is more useful than no data.

Callers wanting readiness poll `status()`, on either handle. There is
deliberately no `wait_until_ready`.

## Cancellation, and who owns the writer task

**Dropping the service does not stop the writer task.** The task holds its own
`Arc<ChainHeadService>`, so the service outlives every handle you keep. Stop it
by cancelling the token you passed to `spawn`, or by calling `shutdown`. A
consumer that just drops the handle leaks the task for the life of the process.

That is the deliberate trade: a writer that stopped when the last handle went
away would stop mid-request in any consumer that briefly holds no subscriber,
and the task has to outlive its handles to publish at all. The consequence is
that the caller owns the lifetime.

Pass a **child token**. A parent token passed directly means shutting down the
chain head shuts down everything else sharing it.

## Publication is all-or-nothing

A snapshot is built as a candidate and installed with one atomic store. A reader
never sees a partially-filled window or a half-applied reorg — the original
published every `depth` blocks mid-catch-up and again when re-anchoring, and
could.

If you are editing the advance path, keep that property. Build into locals;
publish once, at the end, in `publish_snapshot`. Do not store a snapshot from
inside the reorg walk to "make the intermediate state visible".

There is one writer, which is why publication is a plain store rather than a
compare-and-swap. Adding a second writer breaks more than the store: the epoch
bump and the freeze emission both assume exclusive access to what changed.

## Testing: two styles, and which to use

**Stepped** — `spawn_without_writer` + `advance_once`, both compiled out of
production builds. No writer task runs, so the test is the only thing advancing
the graph and observes exactly what it caused. Use this for reorg shapes,
trimming, and freeze emission, where precise sequencing matters and timing does
not.

**Running** — `spawn` the real service against a mock source and observe through
the subscriber with a bounded `wait_for(predicate)`. Use this when the writer
task, the wake handling or the backoff ladder is the thing under test.

Reach for stepped first. A running test that polls for a condition is slower and
can pass for the wrong reason.

`spawn_without_writer` is not a `sync` method under another name — it does not
exist in a production build, and it advances nothing on its own. Do not
"promote" either of these to production visibility for a caller's convenience;
see ADR-0011 for why the chain head cannot be driven from outside.

The `testing` feature is for downstream test crates only, and belongs in
`[dev-dependencies]`. In-crate tests get the same paths from `cfg(test)` without
enabling anything.

## The steady-state early-out

A tick whose tip equals the held tip returns immediately without rebuilding. A
block hash commits to its parent, so an identical tip means an identical chain
beneath it — there is no reorg hiding below a tip both sides agree on.

This is not merely an optimisation to preserve. Without it every poll interval
rebuilds and republishes the whole window, which in tests turned seconds into
minutes.

## Equal-work branches do not displace the incumbent

Best-block selection compares strictly greater-than against the current tip's
work. The original used `max_by_key` over the graph, which returns the last
maximum encountered, so two branches of equal work were ordered by hash-map
iteration order and the winner varied between runs.

If you touch that comparison, keep it strict.

## `MapBackedSnapshot` is an implementation detail

It is this crate's `ChainHeadSnapshot` implementation and the only place the
graph's representation is decided. Consumers name the trait.

Replacing it — persistent structures sharing unchanged subtrees between publishes
rather than maps cloned on each one — is a change to this crate alone, and that
is the arrangement to protect. Do not let a consumer come to depend on the
concrete type.
