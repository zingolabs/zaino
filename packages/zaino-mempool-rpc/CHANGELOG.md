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
- `MempoolService<S, N>`: a single-writer freeze/thaw state machine that mutates
  the transaction set only while the validator tip (V) and non-finalized-state
  tip (NS) agree, re-checking after fetching so an update built against a moved
  tip is discarded; any disagreement, unavailability, or source error freezes and
  serves the last coherent snapshot. States: `NotReady` / `Live` / `Frozen{reason}`
  / `Closing`. See `zaino-mempool/docs/mempool_lifecycle.md`.
- `MempoolSubscriber`: cheap, cloneable, lock-free reads (`snapshot`,
  `get_transaction`, `contains_txid`, `get_txids`, `get_mempool_info`,
  `subscribe_events`), a bounded exclude filter (`validate_exclude_suffixes` +
  `get_filtered_entries`), `stream_raw_transactions`, and a runtime-adjustable
  memory bound (`set_max_cost_bytes`). Supporting types: `MempoolInfo`,
  `TxIdExcludeSuffix`, `MempoolFilterError`.
- Validator-only mode (`MempoolService::spawn_validator_only`): the mempool can
  mirror the validator alone (single-tip), synthesizing the epoch from the
  validator tip.

### Notes
- The live raw-transaction stream closes only when V and NS re-agree at a *new*
  tip (not on a transient freeze), so a client re-calling with the new tip finds a
  matching, live view.
- Serving is lock-free (`ArcSwap` snapshots, shared `Arc` entries, bounded
  broadcast). `std::HashMap` is used deliberately over a persistent (`im::`) map;
  see `zaino-mempool/docs/audit.md`.
