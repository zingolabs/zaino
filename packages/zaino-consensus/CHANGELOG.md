# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. Zcash consensus constants and the protocol-limit validation built
  on them: `COINBASE_MATURITY`, `MAX_BLOCK_REORG_HEIGHT`,
  `MAX_NONFINALISED_DEPTH`, `MAX_BLOCK_BYTES`. Nothing else in the workspace
  should restate these values.
- `work_from_bits` — expands a compact nBits difficulty target and returns the
  work it represents, `floor(2^256 / (target + 1))`, per the protocol
  specification. Rejects malformed targets rather than saturating.

### Changed
- **This crate has no dependencies on any node implementation**, and that is the
  point of it existing separately. A node encodes the consensus rules exactly as
  this crate does; it does not define them. Depending on one to learn a protocol
  constant would take a dependency on a peer's reading of a specification we can
  read ourselves, and drag that peer's entire type system along for a `u32`.
  Each value is stated here with its provenance, and `zaino-convert-zebra` —
  which owns our relationship to zebra's types — carries tests asserting that our
  reading and zebra's still agree, sweeping 256 exponents against 8 mantissas for
  the work derivation. Divergence is a test failure rather than a silent
  behaviour change, and nothing has to depend on zebra to obtain a number.

### Deprecated
### Removed
### Fixed
