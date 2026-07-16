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

### A dedicated crate, separated from the chain state

The mempool is now its own crate, **`zaino-mempool`**, independent of
`zaino-state`. It is a **bounded, coherent, local read model** of the validator's
mempool — explicitly *not* a validator mempool: it does not validate, gossip,
apply fee policy, resolve dependencies, or run its own eviction.

### Hexagonal ports & adapters

The crate depends on nothing in `zaino-state`. Everything it needs from the outside
world it declares as a **port** (a trait it defines itself), and `zaino-state`
supplies the **adapters**. Dependencies point inward: adapters know the core; the
core never names a `zaino-state` type.

- `MempoolSource` — mempool data: `get_mempool_txids` (light diff),
  `get_mempool_metadata` (verbose tip-at-entry heights), `get_raw_mempool_transaction`,
  `get_mempool_source_tip`, and an optional block-wake hint.
- `NfsEpochObserver` — the ChainIndex non-finalized-state epoch (`Option`).
- `zaino-state` provides `MempoolSourceAdapter<S>` over its `BlockchainSource` and
  `NfsEpochAdapter` over its `ArcSwapOption<NonFinalizedState>`, and owns the
  service in `NodeBackedChainIndex`.

This makes the mempool's dependency on the chain state an explicit, minimal
contract rather than an ambient coupling, and lets the crate be tested and reused
in isolation.

### Freeze/thaw dual-tip coherence

Because Zaino reaches the mempool over JSON-RPC (Zebra's in-process
`mempool::FullTransactions` service is not a dependency), there is no atomic
"mempool + its tip" primitive. The read model therefore tracks two tips — the
validator/mempool-source tip (V) and the non-finalized-state tip (NS) — and mutates
its transaction set **only while V and NS agree**, re-checking agreement after
fetching so an update built against a moved tip is discarded. Any disagreement,
unavailability, or source error **freezes** the set: the last coherent snapshot
stays readable and live streams close, until the tips agree again and it
reconciles. This prioritizes ChainIndex/RPC coherence over serving the
freshest-possible mempool during a brief tip transition. See
`packages/zaino-mempool/docs/mempool_lifecycle.md`.

### Protocol-correct internally, wire shape at the boundary

Entries mirror what the validator stores per unconfirmed transaction (Zebra
`VerifiedUnminedTx` / zcashd `nHeight`): the tip-at-entry height, sourced from the
validator, not derived. Wire conversions (`to_lightclient_raw_transaction`,
compact form) live at the boundary; unconfirmed transactions carry wire height `0`.

### Immutable snapshots, shared entries, bounded everything

Serving is lock-free: an immutable `MempoolSnapshot` published behind an `ArcSwap`,
with shared `Arc<MempoolEntry>` (no per-subscriber byte copies) and bounded delta
events over a `tokio::broadcast`. Every client-controllable input and the mempool's
own memory are bounded (exclude-list count/length, per-transaction ZIP-401 cost,
total `max_cost_bytes`). This resolves the AP-05 findings.

### Optional NFS observer (validator-only mode)

The `NfsEpochObserver` is optional. With one (`spawn`), the service enforces
dual-tip coherence — Zaino's production path. Without one
(`spawn_validator_only`), it mirrors the validator alone: NS is synthesized from
the validator tip (a generation counter that advances only on V-hash change), so
coherence collapses to a single tip (freeze on validator-tip change). This
demonstrates and hardens the separation — the mempool genuinely does not require
Zaino's internal state — and makes the dual-tip assumption an explicit, testable
option rather than a hardcoded one.

## Efficiency decisions

- **`std::HashMap`, not a persistent (`im::`) map.** The workload is read-dominated
  with rare, tiny writes; a persistent map would slow the hot `O(1)` read path to
  save microseconds on a ~500 ms write clone. Rejected. (See the audit.)
- **Compact-tx cache** on the shared entry (`OnceCell`), so `GetMempoolTx` parses
  each transaction to compact form at most once, reused across all clients.
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
