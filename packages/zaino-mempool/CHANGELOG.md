# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate: `zaino-mempool`, the hexagonal core of Zaino's mempool subsystem — a
  bounded, coherent, local read model of the validator's mempool, separated from
  `zaino-state` (see `docs/adr/0007-mempool-subsystem-separation.md`). It depends
  on nothing in `zaino-state`; it declares the data it needs as consumer-owned
  ports (`MempoolSource`, optional `NfsEpochObserver`) which `zaino-state` adapts.
- `MempoolService<S, N>`: a single-writer freeze/thaw state machine that mutates
  the transaction set only while the validator tip (V) and non-finalized-state
  tip (NS) agree, re-checking after fetching so an update built against a moved
  tip is discarded; any disagreement, unavailability, or source error freezes and
  serves the last coherent snapshot. States: `NotReady` / `Live` / `Frozen{reason}`
  / `Closing`. See `docs/mempool_lifecycle.md`.
- `MempoolSubscriber`: cheap, cloneable, lock-free reads (`snapshot`,
  `get_transaction`, `contains_txid`, `get_txids`, `get_mempool_info`,
  `subscribe_events`), a bounded exclude filter (`validate_exclude_suffixes` +
  `get_filtered_entries`), `stream_raw_transactions`, and a runtime-adjustable
  memory bound (`set_max_cost_bytes`).
- `MempoolEntry` mirrors the validator's per-transaction data (tip-at-entry
  `entry_height`), with a `to_lightclient_raw_transaction` wire conversion (height
  `0`) and a lazily-computed, shared compact-transaction cache (`compact_tx`).
- `MempoolConfig`: cost-based (ZIP-401) bounds, memory bound (`max_cost_bytes`,
  runtime-adjustable, default 128 MiB), poll interval, fetch concurrency, and
  exclude-list caps.
- Validator-only mode (`MempoolService::spawn_validator_only`, `NoNfs`): the
  mempool can mirror the validator alone (single-tip), synthesizing the epoch from
  the validator tip.

### Notes
- The live raw-transaction stream closes only when V and NS re-agree at a *new*
  tip (not on a transient freeze), so a client re-calling with the new tip finds a
  matching, live view.
- Serving is lock-free (`ArcSwap` snapshots, shared `Arc` entries, bounded
  broadcast). `std::HashMap` is used deliberately over a persistent (`im::`) map;
  see `docs/audit.md`.
