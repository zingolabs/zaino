# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. `ZebraValidator` — the composite holding both Zebra adapters and
  implementing every `zaino-source` port by routing each question to whichever
  transport can answer it.
- Constructors `rpc_only`, `spawn_rpc` and `spawn_direct`, replacing the
  `ValidatorConnector` enum's construction paths.
- Routing for the three mempool sourcing ports (`GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip`), all pinned to JSON-RPC.
  `GetMempoolSourceTip` in particular does *not* use `fast_or_slow!`, unlike its
  `GetChainTip` neighbour: it tags a mempool set read over JSON-RPC, and a tip
  read from the state database would differ by a block for reasons unrelated to
  the mempool, which a consumer would misread as a real tip change.

### Changed
- **`Fetch`-only and `State` are now configurations of one type, not variants
  of an enum.** The composite always holds an RPC adapter and optionally a
  read-state adapter. The old 3,145-line enum matched in every method to answer
  two unrelated questions at once — *can* this transport serve the query, and
  *should* it when both can. Capability is now structural (an adapter that
  cannot answer a question does not implement its port) and preference is a
  routing table.
- Three routing rules, applied per port: read-state where available
  (`fast_or_slow`); read-state first with an RPC fallback on a retryable miss
  (`fast_then_slow`, which preserves `GetBlockByHash`'s side-chain behaviour,
  previously an undocumented per-variant difference); or RPC only.
- `getblockdeltas` and `getaddressdeltas` route to read-state **first**,
  inverting the usual preference, because zebrad does not implement either
  method. On an RPC-only deployment against zebrad they have no answer at all.

### Deprecated
### Removed
### Fixed
