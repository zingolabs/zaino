# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. `zebra-chain` → `zaino-primitives` conversions:
  `block_from_zebra`, `header_from_zebra`, `header_from_parts`,
  `transaction_from_zebra`, and `ConvertError`.

### Changed
- All conversions are fallible. `zebra-chain` types can hold values the domain
  types reject — a height above the protocol maximum, an amount out of range —
  and that check is the purpose of the boundary rather than friction at it.
- One direction only. There is no domain → zebra conversion, and the places
  that still emit `zebra-rpc` shapes build them from block bytes plus chain
  facts using zebra's own builders, so the formatting stays zebra's business.
- Exists so `zebra-chain` appears in exactly one place below the adapters.
  Both the RPC and read-state adapters need the same conversions, and two
  copies drift — which is how a field ends up populated on one transport and
  defaulted on the other.

### Deprecated
### Removed
### Fixed
