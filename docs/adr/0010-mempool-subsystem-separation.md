# ADR 0010: Mempool subsystem separated into `zaino-mempool` behind ports

- Status: accepted
- Date: 2026-08-07

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

- **`zaino-mempool`** — the domain types and the ports. Depends on
  `zaino-primitives` and `zaino-source` (+ `tokio`/`bytes`/`thiserror`); no
  `zaino-state`, no `zaino-proto`, and no node library at all.
- **`zaino-mempool-service`** — the concrete runtime (the polling core service, the
  coherence service, and the read handles). Depends on `zaino-mempool`.

Within that, the subsystem is further split into a **tip-agnostic core** and an
optional **tip-aware coherence** layer (see below), gated by the
`tip_aware_mempool` feature.

### Hexagonal ports & adapters

The crate depends on nothing in `zaino-state`. Dependencies point inward: adapters
know the core; the core never names a `zaino-state` type.

**The validator side is not a bespoke port.** `zaino-source` (ADR 0008) already
defines one trait per question a validator can answer, so the mempool reads those
and names the subset it needs as a consumer-defined bound, `MempoolPorts` —
`GetMempoolTxids` (the light per-poll diff), `GetMempoolMetadata` (the verbose
tip-at-entry heights), `GetRawMempoolTransaction`, `GetMempoolSourceTip`, and
`SubscribeBlocks`. The bound lives in `zaino-mempool` because it states a
requirement of *this consumer*, not a capability of `zaino-source`. Defining a
parallel source trait instead would have restated four questions the port crate
already owns the shape of, and forced a translating adapter that could only ever
lose fidelity.

What *is* declared here is what `zaino-source` cannot describe, because it is a
fact about Zaino rather than about the validator:

- `Mempool` (inbound) — the core's tip-agnostic read model plus the
  `MempoolUpdate` change feed; the coherence layer consumes it.
- `NfsEpochObserver` / `TipAwareMempool` (gated by `tip_aware_mempool`) — the
  ChainIndex non-finalized-state epoch observer the coherence layer needs, and the
  coherent read/stream port it offers.

`zaino-state` supplies `NfsEpochAdapter` over its
`ArcSwapOption<NonFinalizedState>`, plus a thin `MempoolSourceAdapter` whose only
job is to substitute the sync loop's block-arrival wake for the source's (a
request/response validator has no push path). It owns both services in
`NodeBackedChainIndex`.

This makes the mempool's dependency on the chain state an explicit, minimal
contract rather than an ambient coupling, and lets the crate be tested and reused
in isolation.

**The single-source rule.** Every port in `MempoolPorts` must be answered by the
same transport. The core tags each set with `get_mempool_source_tip` so coherence
can judge it without re-fetching, and that comparison is only sound for a
single-source pair. This is why `GetMempoolSourceTip` is a distinct port rather
than a reuse of `GetChainTip`: `ZebraValidator` answers `GetChainTip` from
whichever transport is fastest, preferring the state database, and a tip from
there against a listing from JSON-RPC can differ by a block for reasons that have
nothing to do with the mempool.

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
race this rework closed. See `packages/zaino-mempool/docs/mempool_lifecycle.md`.

### Protocol-correct internally, wire shape at the boundary

Entries hold the full unmined transaction and mirror what the validator stores per
unconfirmed transaction (Zebra `VerifiedUnminedTx` / zcashd `nHeight`): the
tip-at-entry height, sourced from the validator, not derived.

The entry carries **no** parsed or wire form — not a `Transaction`, not a compact
transaction, not a lightclient `RawTransaction`. All of those are derived at the
boundary, and keeping them out is what lets `zaino-mempool` depend on no node
library: it holds the validator's bytes as `Bytes` and never looks inside them.

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
  unmined transaction as bytes; the compact/`RawTransaction` forms are derived on
  demand at the RPC boundary, keeping `zaino-mempool` free of `zaino-proto` and of
  `zebra-chain`. `RawTransaction.data` is generated as `Bytes`, so serving one
  transaction to many streaming clients is a refcount bump each, not a copy each.
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

- The orphaned streamer `JoinHandle` (J-01 bug 2) is tracked under AP-02.
- `BlockchainSource` now requires the four mempool ports as supertraits and no
  longer declares `get_mempool_txids`. That is the migration ADR 0008 describes
  running in reverse for one subsystem: the requirement stays, but the answering
  leaves the wire-typed scaffolding. The rest of `BlockchainSource` follows the
  same path as other subsystems move onto the ports.
