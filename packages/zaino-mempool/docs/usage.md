# Using `zaino-mempool`

A task-oriented guide to the mempool subsystem's **ports and types**. This crate
holds no runtime — the concrete services live in
[`zaino-mempool-rpc`](../../zaino-mempool-rpc/docs/usage.md); read that for spawn /
consume recipes. This guide explains the model and the contracts a consumer or
adapter author must honour.

For *why* the subsystem is shaped this way, see
[ADR-0007](../../../docs/adr/0007-mempool-subsystem-separation.md); for the
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

**Outbound (you implement these; `zaino-state` supplies the adapters):**

- `MempoolSource` — the validator data source: `get_mempool_txids`,
  `get_mempool_metadata` (verbose tip-at-entry heights), `get_raw_mempool_transaction`,
  `get_mempool_source_tip`, and an optional block-wake hint. Must be backed by the
  *same* fetcher that serves the mempool data so the tip and the data are one
  consistent source.
- `NfsEpochObserver` *(feature `tip_aware_mempool`)* — reports Zaino's current
  non-finalized-state epoch (`Option<NonFinalizedEpoch>`); `None` while the NFS
  does not yet exist. `NoNfs` is the no-op for validator-only mode.

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

## Reading the snapshot

`MempoolSnapshot` (from `current()`) is immutable and cheap to hold (`Arc`s
throughout). Key fields: `by_txid` (lookup), `txids_sorted` (reversed-byte order,
for the shortened-txid exclude filter), `entries_in_order`, `tx_count`,
`raw_bytes`, `cost_bytes`, `completeness`, and `source_tip`. Each `MempoolEntry`
holds the full unmined transaction; call `serialized_bytes()` for the raw bytes or
`transaction()` to parse. It carries no RPC/wire forms — derive those (compact tx,
lightclient `RawTransaction` at wire height `0`) at your boundary.

`completeness` tells you whether the set is a full view: `Complete`,
`IncompleteSourceError`, or `IncompleteCapacityLimited`. Never present an incomplete
set as complete on a full-mempool API.

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

## Feature flag

`tip_aware_mempool` (off by default) adds the `NfsEpochObserver` / `TipAwareMempool`
ports, `NonFinalizedEpoch`, the coherent-view types (`CoherentSnapshot`,
`MempoolMode`, `FreezeReason`, `ObservedTips`, `ValidatorTip`, `TipChange`), and the
coherent-stream `MempoolEvent`. Enable it to consume the coherence layer; leave it
off to use the tip-agnostic core standalone.
