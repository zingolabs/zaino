# ADR 0007: Mempool subsystem separated into `zaino-mempool` behind ports

- Status: accepted
- Date: 2026-07-16

## Context

Zaino serves the lightwallet mempool RPCs (`GetMempoolTx`, `GetMempoolStream`,
`getrawmempool`, `getmempoolinfo`, and the mempool arms of `getrawtransaction` /
transaction-status) to many concurrent clients. The previous implementation lived
inside `zaino-state` on a general-purpose `Broadcast` (a shared `DashMap` + a
watch channel) and carried a cluster of confirmed defects (AP-05: unbounded
exclude filter, O(N²) polling, unbounded/duplicated buffers, per-subscriber byte
clones, full-map rescans, a mempool-stream tip race) plus a mempool-height
indexing bug.

We reworked it from the ground up. This ADR records the resulting design and the
decisions behind it.

## Decision

### Dedicated crates, separated from the chain state

The mempool is now its own subsystem, independent of `zaino-state`. It is a
**bounded, coherent, local read model** of the validator's mempool — explicitly
*not* a validator mempool: it does not validate, gossip, apply fee policy, resolve
dependencies, or run its own eviction. Following the hexagonal layering it is split
across two crates:

- **`zaino-mempool`** — the ports (traits) and foundational types. Depends only on
  `zebra-chain` (+ `tokio`/`thiserror`); no `zaino-state`, no `zaino-proto`.
- **`zaino-mempool-rpc`** — the concrete runtime (the polling core service, the
  coherence service, and the read handles). Depends on `zaino-mempool`.

Within that, the subsystem is further split into a **tip-agnostic core** and an
optional **tip-aware coherence** layer (see below), gated by the
`tip_aware_mempool` feature.

### Hexagonal ports & adapters

The crate depends on nothing in `zaino-state`. Everything it needs from the outside
world it declares as a **port** (a trait it defines itself), and `zaino-state`
supplies the **adapters**. Dependencies point inward: adapters know the core; the
core never names a `zaino-state` type.

- `MempoolSource` (outbound) — mempool data: `get_mempool_txids` (light diff),
  `get_mempool_metadata` (verbose tip-at-entry heights), `get_raw_mempool_transaction`,
  `get_mempool_source_tip`, and an optional block-wake hint.
- `Mempool` (inbound) — the core's tip-agnostic read model plus the
  `MempoolUpdate` change feed; the coherence layer consumes it.
- `NfsEpochObserver` / `TipAwareMempool` (gated by `tip_aware_mempool`) — the
  ChainIndex non-finalized-state epoch observer the coherence layer needs, and the
  coherent read/stream port it offers.
- `zaino-state` provides `MempoolSourceAdapter<S>` over its `BlockchainSource` and
  `NfsEpochAdapter` over its `ArcSwapOption<NonFinalizedState>`, and owns both the
  core service and the coherence service in `NodeBackedChainIndex`.

This makes the mempool's dependency on the chain state an explicit, minimal
contract rather than an ambient coupling, and lets the crate be tested and reused
in isolation.

### Tip-agnostic core + tip-aware coherence (freeze/thaw)

Because Zaino reaches the mempool over JSON-RPC (Zebra's in-process
`mempool::FullTransactions` service is not a dependency), there is no atomic
"mempool + its tip" primitive. Coherence with the ChainIndex tip therefore has to
be reconstructed — but it is *separated* from the raw mempool mirror so the two
concerns can be reasoned about (and served) independently:

- **The core (`MempoolService`, always on) never freezes.** It polls the source,
  diffs the set, and always serves the live mempool, so `getrawmempool` /
  `getmempoolinfo` / `GetMempoolTx` reflect reality even mid-transition. It
  **tags** each published snapshot with the validator tip (V) it was fetched at
  (`source_tip`), read from the *same* source that serves the mempool data, and
  discards any poll whose tip moved mid-window (a tag-stability guard). That makes
  `source_tip` a single-source pair with the set.
- **The coherence layer (`CoherenceService`, feature `tip_aware_mempool`) does the
  freeze/thaw.** It consumes the core (`Mempool`) plus the `NfsEpochObserver` (NS)
  and blesses the core's current set as `valid_for` an NS epoch **only while V ==
  NS**. Any disagreement, tip change, unavailability, or an incomplete core set
  **freezes** the coherent view: the last coherent snapshot stays readable and live
  streams keep serving it until the tips re-agree at a new tip. This gates the
  tip-coherent reads (`get_raw_transaction`, `get_transaction_status`) and the raw
  stream, prioritizing ChainIndex/RPC coherence over the freshest-possible mempool
  during a brief transition.

**Why the core must tag V (and why coherence needs no re-fetch).** Freeze/thaw
correctness depends on knowing which validator tip the mempool set was fetched
against. Because the core tags V from the same read that produced the data,
coherence is a *pure function* of `(core set + source_tip, NS)` — comparing V
against NS is sufficient to bless the set, with no re-fetch and no before/after
guards. A fully tip-agnostic core that tagged nothing could not support this: the
set and the tip would come from two independent reads at two instants — the exact
race this rework closed. This is why `get_mempool_source_tip` stays on
`MempoolSource` rather than moving to a separate tip port. See
`packages/zaino-mempool/docs/mempool_lifecycle.md`.

### Protocol-correct internally, wire shape at the boundary

Entries hold the full unmined transaction and mirror what the validator stores per
unconfirmed transaction (Zebra `VerifiedUnminedTx` / zcashd `nHeight`): the
tip-at-entry height, sourced from the validator, not derived. The entry carries
**no** RPC/wire forms; wire conversions (lightclient `RawTransaction` at height
`0`, compact form) are derived at the boundary (the RPC handler for now).

### Immutable snapshots, shared entries, bounded everything

Serving is lock-free: an immutable `MempoolSnapshot` published behind an `ArcSwap`,
with shared `Arc<MempoolEntry>` (no per-subscriber byte copies) and bounded delta
events over a `tokio::broadcast`. Every client-controllable input and the mempool's
own memory are bounded (exclude-list count/length, per-transaction ZIP-401 cost,
total `max_cost_bytes`). This resolves the AP-05 findings.

### Optional NFS observer (validator-only mode)

The `NfsEpochObserver` is optional. With one (`CoherenceService::spawn`), the
coherence layer enforces dual-tip coherence — Zaino's production path. Without one
(`spawn_validator_only`), it tracks the validator alone: NS is synthesized from the
validator tip (a generation counter that advances only on V-hash change), so
coherence collapses to a single tip (freeze on validator-tip change). This
demonstrates and hardens the separation — the mempool genuinely does not require
Zaino's internal state — and makes the dual-tip assumption an explicit, testable
option rather than a hardcoded one.

## Efficiency decisions

- **`std::HashMap`, not a persistent (`im::`) map.** The workload is read-dominated
  with rare, tiny writes; a persistent map would slow the hot `O(1)` read path to
  save microseconds on a ~500 ms write clone. Rejected. (See the audit.)
- **Foundational entry, wire shape at the boundary.** The entry holds only the full
  unmined transaction; the compact/`RawTransaction` forms are derived on demand at
  the RPC boundary, keeping `zaino-mempool` free of `zaino-proto`.
- **Re-fetch-free coherence.** The coherence layer reuses the core's already-fetched
  set (tagged with V); it never issues its own source reads.
- **Light diff + verbose-on-additions**, **binary-search exclude filter**, and a
  **runtime-adjustable memory bound** — see `packages/zaino-mempool/docs/audit.md`.

## Consequences

- The mempool subsystem is independently testable (a full freeze/thaw + concurrency
  matrix in `zaino-mempool`) and independently reusable (validator-only mode).
- `zaino-state` shrinks: the `Broadcast` module and the old mempool are removed.
- A future move of `zaino-state` internals into smaller crates is unblocked for the
  mempool: it already sits behind ports.

## Follow-ups

- Thread a `mempool` config section from `ChainIndexConfig` (the runtime bound is
  already adjustable; file-config wiring is mechanical — see the audit).
- The orphaned streamer `JoinHandle` (J-01 bug 2) is tracked under AP-02.
