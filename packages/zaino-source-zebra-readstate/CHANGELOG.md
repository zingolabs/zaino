# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. `ZebraReadStateAdapter` implements 22 `zaino-source` ports by
  reading Zebra's state database directly, with no RPC round trip. Carries over
  the ~1,100 lines of `ReadStateService` query logic from the deleted
  `ValidatorConnector`, where it is now independently testable.
- `GetBlockDeltas` — derives `getblockdeltas` from block data, resolving each
  input's prevout to recover the spending address and value.

### Changed
- The mempool ports, the passthrough ports, `GetAddressDeltas` and
  `SubscribeChainTip` are deliberately **not** implemented. A state service has
  no mempool, so an implementation of those would silently answer a different
  question than the one asked; under ADR-0008's structural-capability rule,
  routing such a query here is now a compile error rather than a wrong answer.
- Network upgrade names come from serde rather than `Debug`. Zebra's `Display`
  is its `Debug`, and neither matches the RPC surface — the names come from the
  enum's serde renames.

### Deprecated
### Removed
### Fixed
- `getblockdeltas` is served on zebrad-backed deployments. The derivation was
  originally omitted here on the reasoning that it would duplicate "logic the
  validator already has, for no capability gain". That reasoning was false:
  **zebrad does not implement `getblockdeltas` at all**, so this is the only
  implementation such a deployment has, and the method answered `-32601` before
  this. Found by the live suite.
- Network upgrade names no longer disagree between transports: the RPC path
  reported `Nu5` (taken from the validator's reply) while this path reported
  `NU5` (from `Debug`).
