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

## [0.2.0] - 2026-10-09
### Added
- New crate. ChainView: the unified chain read surface, composed from the finalised store below the reorg seam, the chain head's recent window above it, and validator passthrough for what neither indexes. Ships the ports and a composer generic over them, so a deployment bringing its own store or head uses the same crate. Where a tier cannot answer it reports `None` rather than a guess — chainwork above the seam is meaningless until the store has built far enough to say what the anchor's own work was.
- A deployment can offer less than its tiers can answer: `ServedCapabilities` and a type-gated builder, so a capability cannot be advertised by a deployment whose providers cannot supply it. What is served is a decision, not a consequence of what happens to be wired up.
- `ChainViewComposer::spawn_sync` feeds the finalised store from the chain head's freeze stream, building to the height the store reports it is missing. There is no separate catch-up phase: a block from the middle of the chain handed to an empty store answers `FreezeGap`, and closing that gap is the initial sync, so a cold start and a re-anchor after an outage take the same path. Asked for rather than assumed, so a deployment driving its own store does not end up with two writers on one database. The returned handle reports through `StatusSource` and `StatusWatch`.
- Every range read streams in either direction. `stream_blocks`, `stream_raw_blocks` and `stream_compact` run from `start` to `end` inclusive: ascending when `start <= end`, descending when `start > end`, truncated at the chain tip either way. A descending walk plans the same provider segments top-down and reverses each chunk, so it holds no more memory than an ascending one.
### Changed
- `TxOutSetRead::txout_set` answers at the snapshot's tip for the whole chain, not at the finalised watermark. The composer extends the store's accumulator with the chain head's `txout_delta`, resolving the window's spends of older outputs through the store's spend index, and refuses while a hole separates the store from the window. Offering it now needs the store's `SpentOutputs` and `Transactions` indexes as well as `TxOutSet`, and a head snapshot implementing `ChainHeadTxOutSetService`.
- `BlockRead::block_height` no longer reports a height for a block that is off the best chain or at a height this view pins to a different block. For a hash neither tier holds it asks the validator for the block's header and accepts the height only if the validator places it on its best chain at a height no tier covers. `ChainViewSource` now requires `OneShotGetBlockHeader` in place of `OneShotGetBlockByHash`.
- An invalid address or subtree request — an address that does not parse, a range the validator cannot serve, a pool not yet active — is now the new `ChainViewError::Rejected` rather than a zero balance or an empty list. An address never paid is still an ordinary empty answer.
- `ChainViewSnapshot::epoch()` returns the chain state the snapshot is pinned to, so mempool coherence can be checked against the view being read. Implementors must add it.
- dependency `zaino-chain-head` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-store` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
### Fixed
- `ForkReconcile::fork_point` resolves a retained competing-branch hash to the point its branch forks from the canonical chain, via the chain head's own `find_fork_point`, rather than skipping it. A client whose locator names only an orphaned block now gets a resume point instead of `None`.
- Raw blocks are pinned to the snapshot. `BlockRead::raw_block` by height and `stream_raw_blocks` fetch each block the store or chain head covers by the hash the snapshot holds at that height, so a reorg since the snapshot was taken can no longer substitute a block the snapshot never saw. Only heights in a hole are still fetched by height. A pinned block the validator no longer serves is `Transient` rather than a silently short range.
- Compact blocks from the chain head and the validator keep every transaction, as the store's do. Dropping those left empty by the pool filter renumbered the rest, so wallets read the wrong `CompactTx.index` for recent blocks.
- `TreestateRead::treestate` carries each pool's root. The validator's tree port leaves it unset, so the roots are read by the treestate's own block hash and joined in, as `z_gettreestate` reports them.
- A treestate asked for by height is pinned to the snapshot: where the store or chain head covers the height it is fetched by the hash the snapshot holds there, so a reorg since the snapshot was taken cannot substitute another block's trees. Only heights in a hole are still fetched by height.
- `spawn_sync` builds the store up to the chain head's floor on launch, then follows the freeze stream. A store previously came up only once a block was frozen, so on a chain that was not moving — or when the chain head's first freeze was sent before the loop subscribed — it was never built. The loop reports `Ready` once the store's watermark reaches the floor.
- `ChainViewSync` no longer deadlocks when the store rejects a freeze with an error other than a gap, such as `NotReady` during a background build.
