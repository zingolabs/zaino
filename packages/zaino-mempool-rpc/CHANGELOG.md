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
  (over-bound additions dropped, set marked capacity-limited). Implements the
  `zaino-mempool` `Mempool` port via `MempoolSubscriber` and offers a
  `MempoolUpdate` change feed.
- `MempoolSubscriber`: cheap, cloneable, lock-free tip-agnostic reads (`snapshot`,
  `get_transaction`, `contains_txid`, `get_txids`, `get_mempool_info`), the change
  feed as either the raw `subscribe_updates` receiver or the hard-to-misuse
  `mempool_updates()` `Stream` (which surfaces a lag as an in-band
  `MempoolUpdate::Lagged` rather than a silent skip), a bounded exclude filter
  (`validate_exclude_suffixes` + `get_filtered_entries`), and a runtime-adjustable
  memory bound (`set_max_cost_bytes`). Supporting types: `MempoolInfo`,
  `TxIdExcludeSuffix`, `MempoolFilterError`.
- `CoherenceService<M, N>` + `CoherentSubscriber` (feature `tip_aware_mempool`):
  the tip-aware layer. It consumes a `Mempool` core and an `NfsEpochObserver` and
  maintains the `valid_for` freeze/thaw `CoherentSnapshot` — a re-fetch-free pure
  function of `(core set + source_tip, NS)`. Serves the tip-coherent reads and the
  `TipAwareMempool::stream_transactions_until_tip_change` loop (the ready-made
  "stream the mempool until the tip moves" API). Modes: `NotReady` / `Live` /
  `Frozen{reason}` / `Closing`. Validator-only mode (`spawn_validator_only`)
  synthesizes the epoch from the validator tip (single-tip freeze/thaw).

### Notes
- The coherent raw-transaction stream closes only when V and NS re-agree at a *new*
  tip (not on a transient freeze), so a client re-calling with the new tip finds a
  matching, live view.
- Serving is lock-free (`ArcSwap` snapshots, shared `Arc` entries, bounded
  broadcast). `std::HashMap` is used deliberately over a persistent (`im::`) map;
  see `zaino-mempool/docs/audit.md`.
