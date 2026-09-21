# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. Versioned encoding traits and byte helpers, moved verbatim from
  `zaino-state`'s `chain_index/types/encoding.rs`. A leaf: it knows about bytes
  and version tags, and about no domain type whatsoever.
- `ZainoVersionedSerde` — the versioned record format. Every record is a version
  tag byte followed by a version-specific body; a build writes `Self::VERSION`
  and dispatches on the tag when reading, so a newer build reads every older
  row it was taught about.
- `serialize_with_version` / `to_bytes_with_version` — reproduce the exact bytes
  a historical writer produced. Load-bearing rather than a convenience: a
  checksum computed over a nested field's *current* encoding will not verify
  against a row written with an older nested tag, so historical top-level
  encodings must pin their inner versions explicitly.
- `FixedEncodedLen` — for records whose encoded length is a constant, which is
  what lets fixed-width tables be read without a length prefix.
- `CompactSize` and the `read_*` / `write_*` primitives for `u8`…`u64`, `i64`,
  fixed byte arrays, `Option` and `Vec`, in both endiannesses.

### Changed
- No behaviour change. The traits, the helpers and the wire format are the ones
  `zaino-state` already used; the golden vectors checked in before the move pass
  unchanged after it. See ADR-0012.

### Deprecated
### Removed
### Fixed
