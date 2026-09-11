# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.1.0] - 2026-08-28

### Added
- New crate. The domain half of the chain head subsystem: vocabulary and ports
  for the bounded, non-finalised head of the chain, with no runtime and no data
  structures. The runtime is `zaino-chain-head-service`. See ADR-0011.
- `ChainHeadSnapshot` — an immutable view of the retained block graph, and where
  every question about the chain is asked. A **trait**, not a struct: the graph's
  representation belongs to whatever publishes it, so replacing hash maps with
  persistent structures is invisible here. `best_chain_blocks` returns the named
  `ChainHeadBlockIter<'a>` rather than an opaque `impl Iterator`.
- `ChainHeadBlockService` — the read handle: `current()` produces a snapshot,
  `subscribe_updates()` reports when a new one exists. Both total, because a
  chain head finishes initialising before its constructor returns. Its
  associated `Snapshot` type is what keeps the representation out of the port.
- `ChainHeadTransactionService` — transaction location lookups, on the snapshot.
  Both methods are infallible: a transaction that appears nowhere, and an
  outpoint nothing spent, are absence rather than failure.
- `ChainHeadError` — one variant, `InvalidRange`, and `#[non_exhaustive]`. Every
  other snapshot query is a total function of a graph the caller already holds,
  so "not retained" is reported as `None` or an empty collection. A variant is
  added when an implementation exists that can produce it — an implementation
  paging its graph from a store could — rather than in anticipation of one.
- `ChainHeadFreezeEvents` — blocks that have fallen below the consensus seam,
  for a chain store to ingest without re-fetching. A separate trait so a
  consumer bounds on it only when it wants the handoff. The stream is
  best-effort: see ADR-0011 for why gaps are expected rather than exceptional.
- `ChainHeadBlockSource` — the driven port, a bound alias over five
  `zaino-source` ports with a blanket impl, so any source answering all five
  satisfies it without being taught to. Deliberately not `Clone`: a source may
  own connections and a database handle that must not be duplicated.
- `ChainHeadBlock` — a retained block: a `zaino_primitives::Block` with its
  `BlockRef`, its parent, its accumulated work and its commitment tree roots.
  Replaces the persistence type the original graph carried.
- `ChainHeadWork` — accumulated work, **measured from the chain head's own
  anchor rather than from genesis**. It orders competing branches correctly and
  is not the absolute value a validator reports; the distinct name is there so
  the two are not mistaken for each other.
- Epochs are `zaino_primitives::types::ChainStateEpoch`: a generation that
  advances when the canonical tip changes, not on every republication. Readable
  from the runtime handle
  (`ChainHeadBlockService::subscribe_updates`) *and* from a snapshot
  (`ChainHeadSnapshot::epoch`). Both, because a consumer gating on chain state
  needs the epoch of the view it is holding: reading it from the handle instead
  would compare against whatever has been published since, which is the race
  the epoch exists to close. Zaino's mempool coherence layer is that consumer.
- `ChainHeadConfig` — retention window, defaulting to `MAX_NONFINALISED_DEPTH`.
  Fields are private and every knob is a `NonZero` type, so an illegal value is
  unrepresentable rather than caught (or not) at startup. Zero is meaningless
  for all five: a zero window could not observe a reorg, a zero poll interval
  spins against the validator without panicking, a zero backoff defeats the
  retry ladder, and zero tolerated failures is indistinguishable from one. A
  knob where zero *is* meaningful would stay plain.
- `ChainHeadTransparentHistoryService` and the `transparent` module, behind
  `transparent_address_history_experimental`: declared, unimplemented.

### Changed
- There is deliberately **no** `sync`, `sync_to_height`, `reconcile` or
  `advance` on any port here, and no lifecycle port. A chain head synchronises
  itself; a consumer able to drive it could sequence it against something else.
  Starting, stopping and status are inherent methods on the concrete service,
  which reports through `zaino_status::Status` like every other subsystem.
- `ChainHeadBlockSource` does **not** include `GetChainTips`. The chain head
  learns of a competing branch only by living through the reorg that created it,
  so it never asks a validator to enumerate tips. See ADR-0011.

### Deprecated
### Removed
### Fixed
