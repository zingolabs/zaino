# `Quorum<A>` — several validators behind one source

## Why

One validator is one point of failure and, once block decoding is cheap, one
ceiling: a stock validator serving `getblock` spends more CPU than the indexer
reading it. Putting several validators behind the one source the rest of Zaino
already asks fixes both without any consumer learning it happened. The indexer,
the chain head, the serving path and the runtime keep asking "the validator";
configuration knows there are several, and nothing else does.

```text
consumer ─▶ ValidatorClient<Quorum<A>> ─▶ Quorum<A> ─▶ { A₀, A₁, … Aₙ₋₁ }
            retry ladder (outside)          placement (inside)
```

## What it is

A wrapper over N sources of one adapter type, implementing the same `OneShot*`
ports the adapter implements, exactly where it implements them. Not a trait an
adapter opts into: a quorum is a composition of adapters, so it is a type that
composes them, like the two-transport composite in `zaino-source-zebra`. A
`Quorum` of one member behaves as the bare adapter.

## The k-of-n tip

`get_chain_tip` asks every member at once and answers with the `(hash, height)`
at least `k` of them report. Below `k` it fails as a non-domain, retryable
`Connection` failure: the client retries, and if the members stay apart the
consumer sees "unavailable", which is what a source that cannot agree with
itself is.

Two rules keep the answer stable:

- Among several agreeing tips — possible only when `k` is at most half the
  members — the highest wins; at equal height the one matching the previously
  agreed hash, so a stable split does not flap; failing that, the one the
  lowest-indexed member reported, so the choice is deterministic.
- The tip subscription publishes only on agreement. A round below `k` keeps
  the last agreed reading and lets its age carry the news, as the single-source
  poller does. The published tip therefore never moves to a lower height on a
  transient disagreement: it moves backwards only when `k` members report the
  lower tip, which is a reorg, not noise.

A member that answers but disagrees with the quorum is up. It counts for
reachability and is simply outvoted.

## Fetching

By height and by hash the reads are spread: each call starts at the next member
round-robin and, on a non-domain failure or a domain miss, moves to the next.
A miss is returned only when every member reported one; if any member merely
failed, the answer is the failure, because the block may well exist on the
member that could not be reached. Passthrough questions with an authoritative
domain answer — an invalid address, a rejected broadcast, node facts — take
the first reachable member and return its domain answer as given.

The mempool ports are pinned to one member at a time, moving to the next only
when that member fails. The port's single-source rule (a listing, its
transactions and its tip must come from one mempool) holds because they come
from one validator; spreading them would break it.

## Failover is placement, not retry

Moving to the next member is a routing decision made once per call. The wrapper
has no backoff, no attempt counter, no notion of transience: that ladder stays
in `ValidatorClient`, which wraps the quorum from outside, so a consumer holding
a resilient port still holds exactly one retry contract.

## What is deliberately not exposed

- No member identity crosses the port. Which member answered appears in
  tracing fields inside this crate and nowhere else.
- No per-member status, health or endpoint reaches any API. The runtime's
  validator gate sees one source that seeded its tip subscription or did not.
- No new error kind. Below quorum is a non-domain failure with a typed cause
  naming the count; consumers already react to non-domain failures.
- No configuration in this crate: the endpoint list and `k` are the daemon's.
