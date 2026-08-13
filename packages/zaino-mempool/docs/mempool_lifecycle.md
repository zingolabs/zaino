# Mempool lifecycle & states

How Zaino's mempool read model tracks the validator's mempool, which states it
moves through, and how transactions enter and leave it. This is a *read model*: it
mirrors the validator, it does not validate, gossip, or evict on its own policy.

The subsystem is split into two layers:

- **The tip-agnostic core** (`MempoolService`, always on) — mirrors the validator's
  mempool as a live, never-frozen set. Serves `getrawmempool` / `getmempoolinfo` /
  `GetMempoolTx` and the change feed.
- **The tip-aware coherence layer** (`CoherenceService`, feature `tip_aware_mempool`)
  — makes the core set coherent with Zaino's chain tip via freeze/thaw. Serves the
  tip-coherent reads (`get_raw_transaction`, `get_transaction_status`) and the
  raw-transaction stream.

## The two tips

Coherence coordinates two chain tips:

- **V — the validator / mempool-source tip.** The tip of the source that supplies
  mempool data (`getrawmempool` / `getrawtransaction`). The **core** reads it via
  `zaino_source::GetMempoolSourceTip` and **tags every published snapshot** with
  it (`MempoolSnapshot::source_tip`). Mempool transactions are only meaningful
  relative to the tip they are unconfirmed against.
- **NS — the non-finalized-state tip.** Zaino's ChainIndex tip
  (`NfsEpochObserver::current_epoch`, a `(generation, best_tip)` epoch). This is the
  tip Zaino's block reads are served from. Observed only by the **coherence layer**.

Zaino serves *combined* answers (ChainIndex blocks + mempool). If V is ahead of NS,
serving mempool data from V mixed with block data from an older NS is incoherent. So
the coherence layer only blesses the set as coherent while **V and NS agree** (same
tip hash), and freezes otherwise, serving the last coherent set until they agree
again. Because the core tags each set with the V it was fetched at, this is a
re-fetch-free comparison — see the ADR and the `tip` module.

In **validator-only** mode (`spawn_validator_only`) there is no NS; NS is
synthesized from V, so coherence collapses to a single tip — freeze on V change.

## Core lifecycle — always live, never frozen

The core never freezes; it always reflects the latest validator mempool. Its
snapshots carry no freeze/thaw mode, only a `source_tip` tag and a completeness.

The tag doubles as the readiness signal: no tag means no poll has run
(`is_ready()` is false), so completeness is left to describe only the fidelity of
a set that exists. The pre-first-poll snapshot is empty and `Complete` — which
asserts nothing, since `Complete` means "a full view at `source_tip`" and there
is no `source_tip` — and coherence refuses to bless it on the readiness check
rather than on its completeness.

- **`Complete`** — a full view of the validator's mempool at `source_tip`.
- **`IncompleteSourceError`** — a source read failed this poll; the last set is
  retained and marked, and the next poll retries. Never dropped.
- **`IncompleteCapacityLimited`** — applying an addition would breach the cost bound
  (`max_cost_bytes`, the DoS backstop); the addition is dropped and the set marked,
  rather than exceeding the bound or claiming a complete-but-oversized view.

Every set change is published as a bounded [`MempoolUpdate`] change feed —
`Added` / `Removed` / `Reset{sequence}` (the batch boundary) / `Closing`, plus an
in-band `Lagged{missed}` for a consumer that falls behind. The feed is **lossless
at the level of state**: a lagged consumer resyncs from `current()` (see the
`update` module contract). Consume it ergonomically via `mempool_updates()`.

## Coherent view states (tip-aware layer)

```text
                NS/V unavailable
             ┌──────────────────┐
             │     NotReady     │  no coherent view yet
             └────────┬─────────┘
                      │ V and NS known
         ┌────────────▼─────────────┐   V != NS, a tip changed, or
         │          Frozen          │◄──┐ the core set is incomplete
         │  last coherent set kept  │   │
         └────────────┬─────────────┘   │
                      │ V == NS &&        │
                      │ core Complete     │
                      │ (bless core set)  │
         ┌────────────▼─────────────┐    │
         │           Live           │────┘
         │  core set, valid_for NS  │
         └────────────┬─────────────┘
                      │ close()
             ┌────────▼─────────┐
             │     Closing      │
             └──────────────────┘
```

The coherence layer's reconcile is a pure function of `(core set + source_tip, NS)`
with **no re-fetch**:

- **NotReady** — nothing coherent has been published; coherent reads see an empty
  view.
- **Live { valid_for }** — V and NS agree and the core set is `Complete`; the
  coherent view wraps the core's current set, keyed to that NS epoch. New additions
  emit `Added` events on the coherent stream.
- **Frozen { valid_for, reason }** — the coherent view is not advanced; the last
  coherent snapshot stays readable and the coherent stream keeps serving it until
  the tips re-agree. `reason` is one of `ValidatorTipUnavailable` /
  `NonFinalizedUnavailable`, `TipsDiverged` (V and NS known but disagree — the common
  tip-change case), `BothTipsChanged` / `ValidatorTipChanged` /
  `NonFinalizedTipChanged` (coherent transitions momentarily frozen before
  reconciling), or `CoreIncomplete` (the core set is not `Complete` — a source error
  or capacity breach in the core, so it cannot be blessed).
- **Closing** — shutdown; a final coherent snapshot and `Closing` event are
  published.

## How a transaction is ADDED

The **core** applies additions on every poll (interval or block-wake); it does *not*
wait for tip agreement — that is the coherence layer's job:

1. **Tag (before).** Read V — the validator tip this poll's data corresponds to.
2. **Diff.** Fetch the light `getrawmempool` txid list and diff it against the
   current set → `added` / `removed` txids.
3. **Heights.** If there are additions, fetch `getrawmempool verbose` to obtain each
   new transaction's **tip-at-entry height** — the validator's own `nHeight`
   (Zebra `VerifiedUnminedTx.height`, zcashd `CTxMemPoolEntry.nHeight`) — mirroring
   the validator rather than deriving a height locally.
4. **Raw fetch.** Fetch raw bytes for each added txid (bounded concurrency). A txid
   that disappeared between listing and fetch is skipped (a normal race), not an
   error.
5. **Tag-stability guard (after).** Re-read V; if it moved across the fetch window,
   this poll's data is smeared across two tips and cannot be soundly tagged — discard
   and retry next poll. This is what makes `source_tip` a single-source pair with the
   set, so coherence can trust `V == NS` without re-fetching.
6. **Publish.** Swap in a new immutable snapshot tagged with V and emit `Added` /
   `Removed` deltas then a `Reset` batch boundary.

The **coherence layer** then reconciles (on the core's update or its own poll): if
V == NS and the core set is `Complete`, it blesses the set `Live` for that NS epoch
and emits `Added` on the coherent stream.

Wire height for unconfirmed transactions is `0` (the "in the mempool" sentinel,
matching lightwalletd), derived at the RPC boundary; the stored `entry_height` is
protocol metadata, and the consensus branch id for signing uses `tip + 1`.

## How a transaction is EVICTED

Zaino does not run its own eviction policy; a transaction leaves the read model
when:

- **It leaves the validator's mempool** — mined into a block, or evicted by the
  validator (ZIP-401 cost eviction, expiry, or conflict). It disappears from the
  next txid diff and is `Removed` from the next core snapshot. This mirrors Zebra's
  own mempool eviction on `TipAction::Grow` (mined + conflicting + expired) and
  `Reset`.
- **A capacity breach** — if applying an addition would exceed the configured cost
  bound (`max_cost_bytes`, a DoS backstop set above the validator's own ZIP-401
  cap), the addition is not applied; the prior set is kept and the core marks it
  `IncompleteCapacityLimited` (never dropped silently, never claimed complete). The
  coherence layer then holds `Frozen{CoreIncomplete}` until the core is `Complete`
  again.
- **A source error** — the core keeps the last set and marks it
  `IncompleteSourceError`; updates resume when the source recovers. Coherence holds
  `Frozen{CoreIncomplete}` meanwhile.

Note that in the split model, a **tip change never evicts from the core** — the core
stays live across tips; only the *coherent view* freezes until the tips re-agree.

## Cost accounting

Each entry's cost mirrors Zebra's ZIP-401 metric: `max(serialized_size, 10_000)`
bytes. The snapshot's total cost is bounded by `max_cost_bytes` (default 128 MiB,
runtime-adjustable) — deliberately above Zebra's 80 MB `tx_cost_limit` so the
validator's own eviction keeps its mempool under Zaino's cap in healthy operation.

[`MempoolUpdate`]: ../src/update.rs
