# Using `zaino-mempool`

A task-oriented guide to the mempool subsystem's **ports and types**. This crate
holds no runtime — the concrete services live in
[`zaino-mempool-service`](../zaino-mempool-service/usage.md); read that for spawn /
consume recipes. This guide explains the model and the contracts a consumer or
adapter author must honour.

For *why* the subsystem is shaped this way, see
[ADR-0010](../../docs/adr/0010-mempool-subsystem-separation.md); for the
state machine and lifecycle, see [`mempool_lifecycle.md`](./mempool_lifecycle.md).

## The two layers

The mempool is split into two layers, so the fast "what's in the mempool right
now" reads never block on chain-tip transitions:

| Layer | Type | Always on? | Serves |
|---|---|---|---|
| **Tip-agnostic core** | `Mempool` port | yes | the live validator mempool + a change feed — `getrawmempool`, `getmempoolinfo`, `GetMempoolTx` |
| **Tip-aware coherence** | `TipAwareMempool` port | feature `tip_aware_mempool` | the mempool made coherent with Zaino's chain tip — `get_raw_transaction`, `get_transaction_status`, the raw-tx stream |

The core **never freezes**: it mirrors the validator's set as of the last poll and
tags each snapshot with the validator tip it was fetched at
(`MempoolSnapshot::source_tip`). The coherence layer layers freeze/thaw on top,
blessing the core's set as coherent only while the validator tip (V) and Zaino's
non-finalized-state tip (NS) agree.

## Ports

**Outbound — the validator:** not defined here. The core reads the validator
through `zaino-source`'s ports and names the subset it needs as `MempoolSource`:

```rust
pub trait MempoolSource:
    GetMempoolTxids + GetMempoolMetadata + GetRawMempoolTransaction
  + GetMempoolSourceTip + SubscribeBlocks + Clone + Send + Sync + 'static {}
```

A blanket impl means any adapter answering all five earns the bound — nothing
implements `MempoolSource` by name. **The four validator reads must be answered
by the same transport.** The core tags each published set with
`get_mempool_source_tip` so the coherence layer can judge that set without
re-fetching it, and the comparison is only sound for a single-source pair.
`ZebraValidator` upholds this by routing all four to JSON-RPC.

`SubscribeBlocks` is the exception, and is why this is a *capability* bound
rather than a plain source: it is answered by whoever knows a block landed,
which in production is `zaino-state`'s sync loop rather than the validator. It
is a wake *hint*, never a tip source — the tip is re-read from the source on
every tick regardless. It exists because a request/response validator has no
push path, so without a hint the addition latency is always a full poll
interval.

**Outbound — Zaino's own state (you implement this):**

- `NfsEpochObserver` *(feature `tip_aware_mempool`)* — reports Zaino's current
  non-finalized-state epoch (`Option<NonFinalizedEpoch>`); `None` while the NFS
  does not yet exist. Implement `subscribe_epoch_changes` to hand back a
  `watch::Receiver<()>` fired on each publication: without it the coherence layer
  only notices an advance on its next poll tick, which freezes tip-coherent reads
  for that long after every block. `NoNfs` is the no-op for validator-only mode.

**Inbound (implemented by the runtime; you consume these):**

- `Mempool` — the tip-agnostic read model: `current()` (the latest snapshot, the
  authoritative resync source) and `subscribe_updates()` (the change feed).
- `TipAwareMempool` *(feature `tip_aware_mempool`)* — `coherent_snapshot()` and
  `stream_transactions_until_tip_change()`.

## The change feed and its consistency contract

`Mempool::subscribe_updates()` returns a bounded `broadcast::Receiver<MempoolUpdate>`
(`Added` / `Removed` / `Reset{sequence}` / `Lagged{missed}` / `Closing`). It is
bounded, so it scales to many consumers without unbounded buffering — which means
it is **lossless at the level of *state*, not every individual delta**. Two rules
make consuming it safe:

1. **Subscribe before you read `current()`.** Subscribe first, then take your
   starting snapshot, and discard any buffered update whose `sequence` is `<=` that
   snapshot's — so nothing slips through the gap.
2. **On `Lagged`, resync from `current()`.** A consumer that falls behind the
   buffer is told so explicitly (never a silent skip); it must drop its incremental
   state and re-read `current()`. `Reset` is the same resync point after a normal
   republish. `current()` is always the authoritative latest set, so you never lose
   *state* — only intermediate deltas the fresh snapshot already reflects.

The runtime's read handle also offers `mempool_updates()` — an ergonomic `Stream`
that folds the transport lag into an in-band `MempoolUpdate::Lagged`, so rule 2 is
impossible to ignore. Prefer it over the raw receiver.

`event_buffer_len` (in `MempoolConfig`) is a **lag-tolerance** knob, not a
correctness one: it sets how far a consumer may fall behind before it is asked to
resync. State-losslessness does not depend on it.

## Configuring the mempool

`MempoolConfig`'s fields are private: start from `default()` and adjust through
its setters. The knobs with no safe zero take a `NonZero` type, so an illegal
value cannot be constructed rather than being caught (or not) at startup:
`set_poll_interval_ms(NonZeroU64)` — a zero period panics `tokio::time::interval`
at spawn; `set_event_buffer_len(NonZeroUsize)` — zero panics
`broadcast::channel`; `set_max_concurrent_raw_fetches(NonZeroUsize)` — zero
stalls reconciliation instead of throttling it.

The knobs where zero is *meaningful* stay plain, and that distinction is the
point rather than an oversight: `set_metadata_min_interval(Duration)` accepts
zero, meaning "no floor beyond the poll cadence" (it is compared with `>=`), and
`set_max_exclude_count(usize)` accepts zero to disable client-supplied exclusion.

`set_max_cost_bytes` is the exception in the other direction: it takes `&self`,
not `&mut self`, because the bound lives behind a shared atomic so an operator
can move it at runtime across every clone of the config.

## Reading the snapshot

`MempoolSnapshot` (from `current()`) is immutable and cheap to hold (`Arc`s
throughout). Read it through its accessors: `by_txid()` (lookup),
`txids_sorted()` (reversed-byte order, for the shortened-txid exclude filter),
`entries_in_order()`, `tx_count()`, `raw_bytes()`, `cost_bytes()`,
`completeness()`, `unadmitted()`, `source_tip()`, and `is_ready()`.

Construction is sealed — `empty()`, `from_entries()` and `retag()` are the only
ways to build one. The type's invariants (the reversed-byte ordering, the
derived totals agreeing with the set, `unadmitted` empty iff `Complete`) all
fail silently when broken, so the constructor owns them rather than trusting
each call site. You only need this if you are implementing the `Mempool` port
yourself; consumers just read.

Each `MempoolEntry`
holds the full unmined transaction; call `serialized_bytes()` for a borrowed slice,
`wire_bytes()` for the shared `Bytes` buffer (prefer this when handing it to a wire
type — cloning is a refcount bump). It carries no parsed or wire forms at all —
deserialize it, or derive a compact tx or a lightclient `RawTransaction` at wire
height `0`, at your boundary. Keeping those out is what lets this crate depend on
no node library.

`completeness` tells you whether the set is a full view, in two classes. **Short**
(`IncompleteCapacityLimited`, `IncompletePendingMetadata`) — an *accurate* view
that is missing some txids it knows about; `is_whole()` is false but the set is
still safe to serve for positive results. **Possibly-wrong**
(`IncompleteSourceError`) — the set may not reflect the source; `may_be_wrong()`
is true, and this is the only class the coherence layer freezes on. Never present
an incomplete set as complete on a full-mempool API.

`completeness` describes a set that *exists*. Whether one has been built yet is a
separate axis: `is_ready()`, which reads the `source_tip` tag (no tag, no set).
The pre-first-poll snapshot is empty and trivially `Complete`, so `is_whole()`
alone is not a readiness check — the coherence layer refuses to bless an unready
set, and the service's `StatusType` is the operator-facing form of the same
question.

`unadmitted` is the per-txid form of the short case: the exact txids the source
reported that are not in `by_txid` (capacity-refused, or metadata-deferred),
bounded by the txid-listing cap. Use it for precise negative lookups — a queried
txid in `unadmitted` should read as retryable (`Unavailable`), while one that is
simply absent reads as "not found" — rather than gating every absent txid on the
set being short.

## Bounds and back-pressure knobs

Two `MempoolConfig` fields shape how the core behaves under load; both are safety
bounds on Zaino, not validator mempool policy, and both are settable by operators
via `zainod`'s `[mempool]` config section.

- **`max_cost_bytes`** (default 128 MiB) — the ZIP-401 cost ceiling. It bounds the
  *fetch*, not only the retained set: a poll admits at most
  `headroom / MEMPOOL_TRANSACTION_COST_THRESHOLD` additions and refuses the rest
  *without fetching them*, so a non-conforming validator cannot make a poll
  materialise more than the bound. Which additions are admitted is decided on a
  key that is **unpredictable to the sender** (a per-process salted hash of the
  txid within each arrival-time bucket), so a flooding sender cannot grind
  low-sorting txids to displace honest ones from Zaino's view. The snapshot reports
  `IncompleteCapacityLimited` and names the refused txids in `unadmitted`. Refusals
  are remembered so they are not re-fetched every poll, and are retried once the
  set has fallen below a low-water mark *and* has room for that specific
  transaction. Set it on the service (it is not settable through a read handle).
- **`metadata_min_interval`** (default: equal to `poll_interval`) — the floor
  between per-entry metadata listings, which the validator answers by walking its
  whole mempool. Additions are never admitted without their validator-sourced
  metadata, so a poll inside the floor **defers only its additions** (marking the
  set `IncompletePendingMetadata` and listing them in `unadmitted`) while still
  publishing that poll's removals and tip re-tag. Raising it therefore trades
  addition-visibility latency (up to the interval) for load on the validator, and
  carries **no coherence penalty** — because the re-tag still publishes, tip-coherent
  reads thaw after a block on the poll cadence regardless of this value.

## The coherent view (feature `tip_aware_mempool`)

`TipAwareMempool::coherent_snapshot()` returns a `CoherentSnapshot`: the core set
wrapped with a `mode` (`NotReady` / `Live` / `Frozen{reason}` / `Closing`) and the
`valid_for` NS epoch. Combined ChainIndex reads consult it so they only serve the
mempool when it matches the caller's NS snapshot:

- `is_valid_for_snapshot(epoch)` — is the view coherent for this caller's epoch?
- `get(txid)` — the entry, if present in the coherent set.

`stream_transactions_until_tip_change(expected_epoch)` is the ready-made "stream
the mempool until the tip moves" loop: it yields the coherent set's transactions
then each subsequent addition, and closes when the tip changes (re-agrees at a new
epoch) or the service closes. It returns `None` if `expected_epoch` no longer
matches — the caller's tip is stale and should re-snapshot. A *transient* freeze
does not end the stream; the last coherent set stays readable until the tips
re-agree.

Items are `Result<Bytes, MempoolStreamError>`. A consumer that falls behind the
bounded event feed gets `Err(MempoolStreamError::Lagged)` and the stream ends:
**treat that as an incomplete set and re-open against a fresh snapshot**, never as
a normal close. The payload is a shared `Bytes` buffer, so forwarding it to the
wire copies nothing.

A freeze on every block is normal — coherence freezes until the newly-tagged set is
re-blessed, on the order of a poll — so `Frozen` alone is not an alert.
`CoherentSubscriber::frozen_for()` returns how long the view has been *continuously*
frozen (`None` when serving); escalate on a freeze that outlasts normal thaw, which
means tip-coherent reads have gone dark and stayed dark (validator unreachable, NS
stuck). `zaino-state`'s sync loop wires this to the
`zaino.mempool.coherence_frozen_seconds` gauge.

## Feature flag

`tip_aware_mempool` (off by default) adds the `NfsEpochObserver` / `TipAwareMempool`
ports, `NonFinalizedEpoch`, the coherent-view types (`CoherentSnapshot`,
`MempoolMode`, `FreezeReason`, `ObservedTips`, `TipChange`), and the
coherent-stream `MempoolEvent`. Enable it to consume the coherence layer; leave it
off to use the tip-agnostic core standalone.
