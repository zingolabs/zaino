# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Optional `prometheus` feature publishing the set's shape and health from the
  poll loop: `zaino.mempool.transactions`, `zaino.mempool.bytes{kind}`,
  `zaino.mempool.unadmitted`, `zaino.mempool.completeness` (non-zero = a known
  partial view, and the value says why), `zaino.mempool.poll_seconds`. Sampled by
  a `Drop` guard, so the poll's six early returns are covered.

- New crate: `zaino-mempool-service`, the hexagonal *adapter/implementation* layer of
  the mempool subsystem. It supplies the concrete runtime for the ports defined
  in `zaino-mempool`; dependencies point inward (it depends on `zaino-mempool`,
  never the reverse).
- `MempoolService<S>` (tip-agnostic core): a single-writer poll loop that mirrors
  the validator's mempool and **never freezes**, so `getrawmempool` /
  `getmempoolinfo` / `GetMempoolTx` always serve the live set. It tags every
  published snapshot with the validator tip it was fetched at, discarding a poll
  whose tip moved mid-window (a tag-stability guard) so `source_tip` is a
  single-source pair with the set. Enforces the ZIP-401 capacity backstop itself
  (additions admitted in canonical order up to the bound, the rest refused and
  the set marked capacity-limited). Implements the
  `zaino-mempool` `Mempool` port via `MempoolSubscriber` and offers a
  `MempoolUpdate` change feed.
  Generic over `S: MempoolSource`, so it drives off `zaino-source`'s ports
  directly — any adapter answering them can back a mempool, and no bespoke source
  trait or `zaino-state` adapter sits in between.
- `MempoolSubscriber`: cheap, cloneable, lock-free tip-agnostic reads (`snapshot`,
  `get_transaction`, `contains_txid`, `get_txids`, `get_mempool_info`), the change
  feed as either the raw `subscribe_updates` receiver or the hard-to-misuse
  `mempool_updates()` `Stream` (which surfaces a lag as an in-band
  `MempoolUpdate::Lagged` rather than a silent skip), a bounded exclude filter
  (`validate_exclude_suffixes` + `get_filtered_entries`) and a read-only view of
  the memory bound (`max_cost_bytes`). Supporting types: `MempoolInfo`,
  `TxIdExcludeSuffix`, `MempoolFilterError`.
- `CoherenceService<M, N>` + `CoherentSubscriber` (feature `tip_aware_mempool`):
  the tip-aware layer. It consumes a `Mempool` core and an `NfsEpochObserver` and
  maintains the `valid_for` freeze/thaw `CoherentSnapshot` — a re-fetch-free pure
  function of `(core set + source_tip, NS)`. Serves the tip-coherent reads and the
  `TipAwareMempool::stream_transactions_until_tip_change` loop (the ready-made
  "stream the mempool until the tip moves" API). Modes: `NotReady` / `Live` /
  `Frozen{reason}` / `Closing`. Validator-only mode (`spawn_validator_only`)
  synthesizes the epoch from the validator tip (single-tip freeze/thaw).

### Changed
- **Runtime instrumented; source errors no longer swallowed.** Four `Err(_)` arms
  discarded the validator's error (one after building a `MempoolError`) → degraded
  to `IncompleteSourceError` serving a stale set, nothing logged. Now
  edge-triggered (cadence is sub-second): `warn` entering degradation with `cause`
  (failing port / `tip_unstable`) + error, `info` on recovery closing it, `debug`
  while degraded. Tag-stability backstop reports separately (validator answers
  fine; the tip will not hold still).
- One long-lived span per loop (`mempool_poll_loop`, `mempool_coherence_loop`),
  start/stop at `debug`; no per-tick spans (would swamp any trace at this cadence).
- Freeze/thaw carry `FreezeReason` as a structured field, previously
  broadcast-event only → the escalation warning had no *why*, which separates a
  routine block from a diverged validator. Both edges at `debug`: every block
  freezes, so `info` = a line per block on a healthy node. Changed cause logs
  distinctly from entering; an unchanged freeze stays silent.
- `tracing` is this crate's alone — `zaino-mempool` is types, ports, pure functions
  (sole rejection path = `debug_assert`, knobs = `NonZero`): nothing to log, not
  nothing logged.
- Coherence reconciles on the change feed's `Reset` batch boundary only, not every
  per-txid `Added`/`Removed` (clearing a 1,000-tx block = ~2,001 reconciles, each
  re-reading the core snapshot wholesale).
- Snapshot publication maintains `cost_bytes` / `raw_bytes` incrementally, reusing
  collections when only the tip tag moved; that path holds `mempool_generation`
  steady (bumping it made coherence treat every re-tag as new contents).
- Read handles use `ArcSwap::load` where the snapshot `Arc` does not escape (hot
  read paths off the shared refcount).
- `set_max_cost_bytes` moved `MempoolSubscriber` → `MempoolService`: an owner's
  capacity knob, and on the cloneable handle every RPC path holds it was a
  process-wide freeze switch.

### Fixed
- **Capacity refusals no longer loop.** Additions over `max_cost_bytes` were
  dropped, rediscovered next diff → re-fetched every poll, indefinitely. Now
  admitted partially in canonical order, refusals remembered with their cost
  (fetched once). Retried only below a 90% low-water mark **and** with room for
  that exact transaction — hysteresis + fit check, so a retry cannot re-refuse.
- **Metadata listing rate-floored** by the new
  `MempoolConfig::metadata_min_interval`: a poll finding additions inside the floor
  publishes *nothing*, never a set missing them (the coherence layer would bless it).
- **Coherent stream no longer ends silently on a lag** — yields
  `Err(MempoolStreamError::Lagged)` first (a quiet end was indistinguishable from
  the normal tip-change close → clients took a partial mempool for a complete one).
- **Post-block coherence blackout** — the layer now selects on the NS-epoch
  observer's wake signal, reconciling when non-finalized state advances, not on the
  next poll tick (kept as a fallback).
- **A poll no longer does all its work before discarding it** — tag-stability guard
  runs *before* the metadata listing and raw fetches; after
  `MAX_CONSECUTIVE_DISCARDS` (5) in a row the set republishes as
  `IncompleteSourceError` (consumers learn it is not converging).

### Notes
- The coherent raw-transaction stream closes only when V and NS re-agree at a *new*
  tip (not on a transient freeze), so a client re-calling with the new tip finds a
  matching, live view.
- Serving is lock-free (`ArcSwap` snapshots, shared `Arc` entries, bounded
  broadcast). `std::HashMap` is used deliberately over a persistent (`im::`) map;
  see `zaino-mempool/docs/audit.md`.
