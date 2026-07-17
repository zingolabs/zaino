# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate: `zaino-mempool`, the hexagonal *ports + foundational types* of
  Zaino's mempool subsystem — a bounded, coherent, local read model of the
  validator's mempool, separated from `zaino-state` (see
  `docs/adr/0007-mempool-subsystem-separation.md`). It depends on nothing in
  `zaino-state`; it declares the data it needs as consumer-owned ports
  (`MempoolSource`, optional `NfsEpochObserver`) which `zaino-state` adapts. The
  concrete runtime (the polling service and the read handle) lives one layer out
  in `zaino-mempool-rpc`.
- Port traits: `MempoolSource` (validator data source) and `NfsEpochObserver`
  (optional non-finalized-state epoch observer, with the `NoNfs` no-op).
- Foundational types: `MempoolEntry`, `MempoolConfig`, `MempoolError`,
  `MempoolEvent`, `MempoolSnapshot` (with `MempoolMode` / `FreezeReason` /
  `MempoolCompleteness` / `ObservedTips` / `ValidatorTip` / `TipChange`),
  `MempoolTxMeta`, `BlockRef`, `NonFinalizedEpoch`, and the `SendFut` alias.
- `MempoolEntry` holds the full unmined transaction (serialized bytes + protocol
  metadata, tip-at-entry `entry_height`) and exposes foundational parse
  accessors (`serialized_bytes`, `transaction()`). It carries **no** RPC/wire
  forms: the compact-transaction cache and `to_lightclient_raw_transaction` were
  removed, and the `zaino-proto` / `once_cell` dependencies dropped. Wire
  conversions move to the boundary (the RPC handler for now).
- `MempoolConfig`: cost-based (ZIP-401) bounds, memory bound (`max_cost_bytes`,
  runtime-adjustable, default 128 MiB), poll interval, fetch concurrency, and
  exclude-list caps.
