# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. Zcash address classification for `validateaddress` and
  `z_validateaddress`: `validate_address`, `z_validate_address`,
  `sapling_key_bytes`, and the domain types `ValidatedAddress` /
  `ZValidatedAddress` / `AddressKind`.
- `DEPRECATION_NOTICE`, for the serving layer to log on every
  `z_validateaddress` call. `validateaddress` is not deprecated and carries no
  notice.

### Changed
- Moved out of `zaino-state::indexer`, where it sat as chain-free logic inside
  a chain-access crate. As a leaf it isolates a dependency set
  (`zcash_address`, `zcash_keys`, `zcash_transparent`, `sapling-crypto`) that
  nothing else in Zaino wants — too heavy for `zaino-primitives`, and domain
  logic rather than infrastructure, so not `zaino-common`.
- No serde. The zcashd-shaped JSON, whose field set differs per address kind,
  is `zaino-serve`'s `wire/address.rs` per ADR-0009.
- Sprout is reported invalid, and `ZValidatedAddress` has no Sprout variant.
  This matches existing behaviour rather than changing it — the previous
  classifier already fell through to `invalid()` for Sprout, with a comment
  saying support was disabled. What changed is that the type now says so
  instead of a dead wire variant implying support the classifier never
  produced.
- `ismine` is never emitted. It reports whether the *node's wallet* holds the
  key; Zaino has no wallet, so it has no answer, and emitting `false` would be
  a claim rather than an omission.

### Deprecated
### Removed
### Fixed
