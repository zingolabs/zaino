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

## [0.2.0] - 2026-09-26
### Added
- The catch-up walk fetches each block and its tree roots concurrently, through the height-addressed `OneShotGetCommitmentTreeRootsByHeight` port. The two reads can straddle a reorg, so the walk compares the hash that the roots answer names against the block it fetched, and refetches the roots by hash on a mismatch.
### Changed
- `ChainHeadAdvanceError::SourceUnavailable` carries the source's `FetchError` instead of a `String`, so `Error::source()` returns the transport cause and its `FailureMode`. A rejected query stays under `InconsistentSource`, naming the query.
  _Migration:_ Match `SourceUnavailable(fetch_error)` and format the error with `Display` where a message was read from the `String`.
- The `prometheus` feature and the implicit `metrics` feature are removed; reorg metrics are always emitted and cost nothing without an installed recorder.
  _Migration:_ Remove `prometheus` or `metrics` from this crate's feature list in dependent manifests.
- dependency `zaino-chain-head` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-primitives` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
### Fixed
- The chain head now follows the validator when its tip moves to a lower block. Previously a retained block with more work pulled the tip back up, so a rollback was never followed. A heavier retained block is now logged as a warning instead.
- After a rollback or a reorg onto a shorter branch, heights above the new tip are no longer reported as part of the best chain. The blocks there stay available as a competing branch.
- A block from the validator that does not attach directly to the chain head's tip (wrong height or wrong parent) now fails the update as inconsistent validator data, instead of being added to the best chain.
- Reorg handling and reorg detection no longer recurse once per block. A reorg deeper than roughly 130 blocks, or a validator rollback below the retained window, overflowed the stack and aborted the process; both now walk iteratively and terminate at the retained window. (#1551)
- Every block that crosses the consensus seam is handed off to the freeze stream. Trimming ran before the handoff read the blocks it emits, so an iteration advancing the tip by more than eleven blocks settled blocks it never handed off, and reported nothing.
- The chain head follows the validator's tip, including a tip that rolls back, and refuses a block labelled with a height other than the next one. Before, the heaviest retained block won and a mislabelled block joined the best chain. The previous rule stays as a crate-private policy, TipSelection::HeaviestRetained.
- `zaino.sync.reorg_total` / `reorg_depth` now count reorgs won by a longer chain (previously missed by a height comparison); depth = blocks rewritten back to the fork point. An old tip that an advance pushed below the retained window, or that a re-anchor dropped, is an advance and not a reorg; only an old tip still inside the window counts as a reorg of unknown depth. Feature `prometheus` removed; emission is unconditional.
### Internal
- The in-memory chain head stores its tip block separately from the other retained blocks, so it can never be empty.
- The snapshot keeps its own invariants in the type: the tip is a field, so the graph is never empty; a block becomes canonical only as a checked child of the tip; a branch switch rewinds to the fork point first. A crate-private ChainGraph contract, with a model-based property test, runs against every implementation.
- The snapshot's tip lookup goes through a single `tip_block` accessor. (#1482)
- Value balances use the checked zatoshi quantity types.
- Fold a window's work from RelativeChainWork::ZERO at the anchor. A work overflow while extending the window is reported as ChainHeadAdvanceError::InconsistentSource rather than ReorgFailure.
- The extending block's work is proven non-zero on the NonZeroU128 step before SingleBlockWork::new, with the same error message as before.

## [0.1.1] - 2026-09-11

### Added
### Changed
### Deprecated
### Removed
### Fixed
- Reading past the chain tip treats only the source's named not-found
  errors (`GetBlockError::HeightNotFound`, `GetBlockByHashError::NotFound`)
  as absence, so a future domain error variant fails to compile there
  instead of being read as the end of the chain. Each absence is logged at
  debug level.

## [0.1.0] - 2026-08-28

### Added
- New crate. The chain head runtime: carries over the synchronisation, reorg
  handling and retention logic from `zaino-state`'s
  `chain_index/non_finalised_state.rs`, now owning its own writer task. See
  ADR-0011.
- `ChainHeadService` — anchors a complete window before `spawn` returns, then
  keeps it reconciled with the validator. Public surface is `spawn`,
  `subscriber`, `status`, `shutdown` and `impl zaino_status::Status`; every
  method that advances the graph is private.
- `ChainHeadSubscriber` — the read handle. Cheap to clone, and holds no ability
  to drive or stop synchronisation. It holds the published cell rather than a
  snapshot taken from it, so a subscriber kept for the process lifetime keeps
  seeing the chain move. It also reports the runtime's status
  (`impl zaino_status::Status`) from a clone of the service's own cell, so both
  handles observe one set of transitions: a published snapshot looks the same
  whether the writer is keeping up or has given up, and a consumer holding only
  this handle still has to be able to say whether the tip it is being served is
  fresh.
- `MapBackedSnapshot` — this crate's `ChainHeadSnapshot` implementation, and the
  only place the graph's representation is decided. It carries the generation of
  the publication that produced it, stamped before the snapshot is stored, so a
  captured view keeps reporting the epoch it was published under rather than
  following the chain head forward. The generation field is private and written
  only by `stamp_generation`, which owns the rule: a publication whose tip is
  unchanged inherits the previous generation, and one whose tip moved advances
  past the highest yet published. Taking the highest published rather than the
  previous snapshot's is what keeps it monotonic across a re-anchor, where the
  graph is rebuilt from a single block and starts at zero.
- `ChainHeadAdvanceError` (`SourceUnavailable` / `InconsistentSource` /
  `ReorgFailure`) and `ChainHeadInitError`, for a chain head that could not
  anchor. `SourceUnavailable` carries the transport `FetchError` as its
  `#[source]`, so `Error::source()` yields the underlying cause — and its
  machine-readable `FailureMode` — instead of a flattened string; a source
  `QueryError` is classified at the boundary, transport failures becoming
  `SourceUnavailable` and domain rejections staying message-only under
  `InconsistentSource`.
- Reorg metrics, moved here from `zaino-state` with their existing metric
  strings unchanged, behind the `prometheus` feature.
- `spawn_without_writer` and `advance_once`, behind `#[cfg(any(test, feature =
  "testing"))]` and compiled out of production builds entirely, for tests that
  need deterministic stepping. `spawn_without_writer` shares the anchoring path
  with `spawn` and differs only in whether the task is spawned, so the two
  cannot drift.

### Changed
- Publication is atomic. A candidate snapshot is built to completion and
  installed with a single store, where the original wrote into the shared cell
  every `depth` blocks mid-catch-up and again when re-anchoring. A reader can no
  longer observe a partially-filled window or a half-applied reorg.
- The reorg walk mutates working locals and the snapshot is constructed once, at
  the end of the tick, rather than being threaded through the walk as `&mut`.
- The compare-and-swap and the `initial_state` threading it required are gone.
  There is one writer, so there is no race to lose.
- The anchor and trim floors come from `tip - max_depth` rather than the
  finalised state's `db_height()` — the arm the original already took whenever
  the database lagged.
- A tick whose tip matches the held tip returns without rebuilding. A block hash
  commits to its parent, so an identical tip means an identical chain beneath
  it, and there is no reorg hiding below a tip both sides agree on.
- The epoch advances only when the canonical tip changes, so a consumer pinned
  to one is woken when the chain actually moved.
- Frozen blocks are collected only when something is subscribed, so a deployment
  not using the handoff pays nothing for it.

### Deprecated
### Removed
- The block-carrying change listener — `nonfinalized_listener`, `setup_listener`,
  `handle_nfs_change_listener` and `add_nonbest_block`. Every production source
  returned `Ok(None)`, so the handler early-returned and the code behind it was
  unreachable.
- `SyncError::CannotReadFinalizedState` and `UpdateError::StagingChannelClosed`,
  whose collaborators no longer exist, and `StaleSnapshot`, which described the
  lost CAS race.

### Fixed
- Competing branches of equal work no longer resolve by hash-map iteration
  order. The original selected the best block with `max_by_key`, which returns
  the last maximum encountered, so a reorg to a same-height branch of equal work
  was decided by whichever order the map happened to yield — the choice was
  unstable between runs. The comparison is now strictly greater-than against the
  current tip's work, so an equal-work branch does not displace the incumbent.
