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
  `docs/adr/0010-mempool-subsystem-separation.md`). It depends on nothing in
  `zaino-state`; it declares the data it needs as consumer-owned ports which
  `zaino-state` adapts. The concrete runtime lives one layer out in
  `zaino-mempool-service`.
- The subsystem is split into a **tip-agnostic core** and an optional
  **tip-aware coherence** layer (feature `tip_aware_mempool`, off by default):
  - Always available: `MempoolSource` (the `zaino-source` subset the core reads
    the validator through); the `Mempool` inbound port (the core's tip-agnostic
    read model plus the `MempoolUpdate` change feed); `MempoolSnapshot` — now
    tip-agnostic, tagged with the validator tip (`source_tip`) the set was
    fetched at; `MempoolEntry`, `MempoolConfig`, `MempoolError`,
    `MempoolCompleteness`, and `MempoolUpdate`. The tip tag is
    `zaino_primitives::types::BlockRef` — chain-wide vocabulary, not this
    crate's, so there is one canonical type rather than a mempool-local copy.
  - Under `tip_aware_mempool`: the `NfsEpochObserver` port (with `NoNfs`), the
    `TipAwareMempool` port (`coherent_snapshot` + the ready-made
    `stream_transactions_until_tip_change` loop), `NonFinalizedEpoch`, the
    coherent-view types (`CoherentSnapshot`, `MempoolMode`, `FreezeReason`,
    `ObservedTips`, `ValidatorTip`, `TipChange`), and the coherent-stream
    `MempoolEvent`.
- The `MempoolUpdate` change feed (`Added` / `Removed` / `Reset{sequence}` /
  `Lagged{missed}` / `Closing`) carries only small facts — `Reset` is a batch
  boundary that points consumers at `current()`, never the snapshot itself — so
  buffered updates stay tiny under many subscribers. It is **lossless at the
  level of state**: a consumer that falls behind the bounded feed gets an
  explicit `Lagged` (never a silent skip) and resyncs from `current()`. See the
  `update` module docs for the subscribe-before-read / resync-on-lag contract.
- `MempoolEntry` holds the full unmined transaction (serialized bytes + protocol
  metadata, tip-at-entry `entry_height`) and exposes foundational parse
  accessors (`serialized_bytes`, `transaction()`). It carries **no** RPC/wire
  forms: the compact-transaction cache and `to_lightclient_raw_transaction` were
  removed, and the `zaino-proto` / `once_cell` dependencies dropped. Wire
  conversions move to the boundary (the RPC handler for now).
- `MempoolConfig`: cost-based (ZIP-401) bounds, memory bound (`max_cost_bytes`,
  runtime-adjustable, default 128 MiB), poll interval, fetch concurrency, and
  exclude-list caps.
- `NfsEpochObserver::subscribe_epoch_changes` — an optional wake signal (default
  `None`) fired when a new non-finalized snapshot is published, so the coherence
  layer reconciles on the advance instead of waiting out its poll tick. Without
  it, tip-coherent reads were frozen for a poll interval after every block, and
  indefinitely when sync lagged.
- `MempoolStreamError` (feature `tip_aware_mempool`) — why a tip-coherent stream
  ended early. `TipAwareMempool::stream_transactions_until_tip_change` now yields
  `Result<Bytes, MempoolStreamError>`: a consumer that falls behind the event feed
  gets `Lagged` instead of a silent end, which was indistinguishable from the
  normal tip-change close and so let a partial mempool pass for the whole one.
- `MempoolEntry::wire_bytes` — the transaction as a shared `Bytes` buffer.
- `MempoolConfig::metadata_min_interval` — a floor between per-entry metadata
  listings (`getrawmempool verbose`), which the validator answers by walking its
  whole mempool. Defaults to `poll_interval`, i.e. no additional coalescing;
  raising it trades mempool latency for validator load. Additions are never
  admitted without their metadata, so a poll inside the floor publishes nothing
  rather than an incomplete set. `DEFAULT_POLL_INTERVAL` is now a public constant.

### Changed
- The validator bound is `MempoolSource`, not `MempoolPorts`. A trait bound
  reads best as the capability a satisfying type has — `impl<S: MempoolSource>`
  says the thing can source a mempool — rather than as its place in the
  architecture. `zaino-state`'s `ChainIndexSourcePorts` is the same construct
  under the older convention; that crate is transitional wiring being retired as
  its subsystems move out, and each one that lands takes the capability name.
- `MempoolEntry::serialized_tx` is a `bytes::Bytes` (was `Arc<SerializedTransaction>`),
  built once at ingest and shared to the wire, so fanning one transaction out to
  N stream consumers costs N refcount bumps rather than N copies.
- `MempoolEntry::raw_len` and `tx_cost` take `u64` (was `u32`), which could
  silently wrap on a narrowing cast at ingest.

### Fixed
- `NonFinalizedEpoch::generation` documentation: it increments when the
  publisher's best tip *changes*, not on every republication. The code was
  already correct; the doc claimed the opposite.

### Notes
- **Why the core tags `source_tip`.** Freeze/thaw coherence depends on knowing
  which validator tip a mempool set was fetched against. The core reads that tip
  from the *same* source that serves the mempool data and stamps it on every
  snapshot, so the coherence layer decides `V == NS` without re-fetching. A
  fully tip-agnostic core that tagged nothing could not support sound downstream
  coherence — the set and the tip would come from two independent reads (the race
  the rework closed). See the `tip` module and `zaino-mempool-service`'s coherence
  service.
