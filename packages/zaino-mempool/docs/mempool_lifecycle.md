# Mempool lifecycle & states

How Zaino's mempool read model tracks the validator's mempool, which states it
moves through, and how transactions enter and leave it. This is a *read model*: it
mirrors the validator, it does not validate, gossip, or evict on its own policy.

## The two tips

The mempool coordinates two chain tips:

- **V — the validator / mempool-source tip.** The tip of the source that supplies
  mempool data (`getrawmempool` / `getrawtransaction`). Observed via
  `MempoolSource::get_mempool_source_tip`. Mempool transactions are only meaningful
  relative to the tip they are unconfirmed against.
- **NS — the non-finalized-state tip.** Zaino's ChainIndex tip
  (`NfsEpochObserver::current_epoch`, an `(generation, best_tip)` epoch). This is
  the tip Zaino's block reads are served from.

Zaino serves *combined* answers (ChainIndex blocks + mempool). If V is ahead of NS,
serving mempool data from V mixed with block data from an older NS is incoherent.
So the mempool only mutates its set while **V and NS agree** (same tip hash), and
freezes otherwise, serving the last coherent set until they agree again.

In **validator-only** mode (`spawn_validator_only`) there is no NS; NS is
synthesized from V, so coherence collapses to a single tip — freeze on V change.

## States

```text
                NS/V unavailable
             ┌──────────────────┐
             │     NotReady     │  no coherent set yet
             └────────┬─────────┘
                      │ V and NS known
         ┌────────────▼─────────────┐   V != NS, a tip changed,
         │          Frozen          │◄──┐ source error, or capacity
         │  last coherent set kept  │   │ breach
         └────────────┬─────────────┘   │
                      │ V == NS          │
                      │ (reconcile:      │
                      │  fetch + diff)   │
         ┌────────────▼─────────────┐    │
         │           Live           │────┘
         │   set == validator @ V   │
         └────────────┬─────────────┘
                      │ close()
             ┌────────▼─────────┐
             │     Closing      │
             └──────────────────┘
```

- **NotReady** — nothing coherent has been published; reads see an empty set.
- **Live { valid_for }** — V and NS agree; the set equals the validator's mempool
  at that epoch. Transaction additions/removals are applied and delta events emit.
- **Frozen { valid_for, reason }** — the set is not mutated; the last coherent
  snapshot stays readable and live delta streams close. `reason` is one of
  `ValidatorTipUnavailable` / `NonFinalizedUnavailable`, `TipsDiverged` (V and NS
  known but disagree — the common tip-change case), `BothTipsChanged` /
  `ValidatorTipChanged` / `NonFinalizedTipChanged` (coherent transitions momentarily
  frozen before reconciling), `SourceError`, or `CapacityLimited`.
- **Closing** — shutdown; a final snapshot and `Closing` event are published.

Completeness travels with the snapshot: `Complete`, `IncompleteSourceError`, or
`IncompleteCapacityLimited` — full-mempool APIs must never present an incomplete
set as complete.

## How a transaction is ADDED

Per poll (interval or block-wake), while V and NS agree on epoch `E`:

1. **Coherence guard (before).** Re-observe V and NS; abort if they no longer agree
   on `E`.
2. **Diff.** Fetch the light `getrawmempool` txid list and diff it against the
   current set → `added` / `removed` txids.
3. **Heights.** If there are additions, fetch `getrawmempool verbose` to obtain each
   new transaction's **tip-at-entry height** — the validator's own `nHeight`
   (Zebra `VerifiedUnminedTx.height`, zcashd `CTxMemPoolEntry.nHeight`) — mirroring
   the validator rather than deriving a height locally.
4. **Raw fetch.** Fetch raw bytes for each added txid (bounded concurrency). A txid
   that disappeared between listing and fetch is skipped (a normal race), not an
   error.
5. **Coherence guard (after).** Re-observe V and NS; if they no longer agree on `E`,
   discard all fetched work and freeze — an update built against a tip that moved
   mid-fetch is never published.
6. **Publish.** Swap in a new immutable snapshot and emit `Added` (and `Removed`)
   delta events, then a `Live` event.

Wire height for unconfirmed transactions is `0` (the "in the mempool" sentinel,
matching lightwalletd); the stored `entry_height` is protocol metadata, and the
consensus branch id for signing uses `tip + 1`.

## How a transaction is EVICTED

Zaino does not run its own eviction policy; a transaction leaves the read model
when:

- **It leaves the validator's mempool** — mined into a block, or evicted by the
  validator (ZIP-401 cost eviction, expiry, or conflict). It disappears from the
  next txid diff and is `Removed` from the next Live snapshot. This mirrors Zebra's
  own mempool eviction on `TipAction::Grow` (mined + conflicting + expired) and
  `Reset`.
- **A tip changes** — the whole set freezes (it is stale relative to the new tip);
  it is re-reconciled against the validator once the tips agree at the new tip.
- **A capacity breach** — if applying an update would exceed the configured cost
  bound (`max_cost_bytes`, a DoS backstop set above the validator's own ZIP-401
  cap), the update is not applied; the prior set is kept and marked
  `IncompleteCapacityLimited` (never dropped silently, never claimed complete).
- **A source error** — the set freezes as `IncompleteSourceError`; the last
  coherent set stays readable and updates resume when the source recovers.

## Cost accounting

Each entry's cost mirrors Zebra's ZIP-401 metric: `max(serialized_size, 10_000)`
bytes. The snapshot's total cost is bounded by `max_cost_bytes` (default 128 MiB,
runtime-adjustable) — deliberately above Zebra's 80 MB `tx_cost_limit` so the
validator's own eviction keeps its mempool under Zaino's cap in healthy operation.
