# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate: `zaino-mempool-rpc`, the hexagonal *adapter/implementation* layer of
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

### Fixed
- **Capacity refusals no longer loop.** Additions over `max_cost_bytes` were all
  dropped and then rediscovered by the next diff, so refused transactions were
  re-fetched from the validator on *every* poll, indefinitely, while the set
  stayed capacity-limited. Additions are now admitted partially in canonical
  order, and the refusals are remembered (with their cost) so they are fetched
  once. They are retried only when the set has both fallen below a 90% low-water
  mark and freed enough room for that specific transaction — hysteresis plus an
  exact fit check, so a retry can never end in an immediate re-refusal.
- **The metadata listing is rate-floored** by the new
  `MempoolConfig::metadata_min_interval`. A poll that finds additions inside the
  floor publishes *nothing* rather than a set missing them: an incomplete view
  must never be published as complete, or the coherence layer would bless it.
- **The coherent stream no longer ends silently on a lag.** It yields
  `Err(MempoolStreamError::Lagged)` first; ending quietly was indistinguishable
  from the normal tip-change close, so a client took a partial mempool for the
  complete one.
- **Post-block coherence blackout.** The coherence layer now selects on the
  NS-epoch observer's wake signal, so it reconciles when the non-finalized state
  advances rather than on its next poll tick. The tick remains a fallback.
- **A poll no longer does all its work before discarding it.** The tag-stability
  guard also runs *before* the metadata listing and raw fetches, and after
  `MAX_CONSECUTIVE_DISCARDS` (5) discards in a row the set is republished as
  `IncompleteSourceError` so consumers learn the mempool is not converging.

### Changed
- The coherence layer reconciles on the change feed's `Reset` batch boundary
  only, not on every per-txid `Added`/`Removed`. Clearing a block of 1,000
  transactions previously meant ~2,001 reconciles, each of which re-read the
  core's snapshot wholesale anyway.
- Snapshot publication maintains `cost_bytes` / `raw_bytes` incrementally and
  reuses the existing collections when only the tip tag moved — that path also
  keeps `mempool_generation` steady, where bumping it made the coherence layer
  treat every re-tag as new contents.
- Read handles use `ArcSwap::load` where the snapshot `Arc` does not escape,
  keeping the hot read paths off the shared refcount.
- `set_max_cost_bytes` moved from `MempoolSubscriber` to `MempoolService`. It is a
  capacity-control knob for the mempool's owner; on the cloneable read handle that
  every RPC path holds, it was effectively a process-wide freeze switch.

### Notes
- The coherent raw-transaction stream closes only when V and NS re-agree at a *new*
  tip (not on a transient freeze), so a client re-calling with the new tip finds a
  matching, live view.
- Serving is lock-free (`ArcSwap` snapshots, shared `Arc` entries, bounded
  broadcast). `std::HashMap` is used deliberately over a persistent (`im::`) map;
  see `zaino-mempool/docs/audit.md`.
