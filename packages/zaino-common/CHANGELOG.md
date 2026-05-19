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

## [0.1.1] - 2026-05-19

### Changed
- `LogConfig::default` color auto-detection now uses
  `std::io::stderr().is_terminal()` instead of the `atty` crate (#1020).
  Behavior is unchanged; the `atty` dependency is dropped.

## [0.1.0] - 2025-09-09

Initial release. `zaino-common` is the foundation crate shared by the
other zaino crates, extracted from `zaino-state` in PR #524
(`zaino-commons-foundation`). The crate version sat at `0.1.0` in
`Cargo.toml` from extraction through the 0.3.0 development cycle and
was never published to crates.io; the surface described below is what
ended up in the 0.1.0 line by the time `0.1.1` was cut.

### Added

#### `config` module
- `config::network`: `Network`, `ActivationHeights`,
  `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS`.
- `config::service`: `ServiceConfig`.
- `config::storage`: `StorageConfig`, `CacheConfig`, `DatabaseConfig`,
  `DatabaseSize` (XDG-cache-aware default paths added in #784/#854).
- `config::validator`: `ValidatorConfig` with a grpc-port-extraction
  helper.
- Migration from `figment` to `config-rs` for TOML loading (#469).

#### `logging` module (#888)
- `LogConfig`, `LogFormat`, plus the helpers `init`, `try_init`,
  `init_with_config`, `try_init_with_config`.
- `DisplayHash`, `DisplayHexStr` display wrappers.

#### `net` module
- `resolve_socket_addr`, `try_resolve_address`, `AddressResolution`
  (DNS hostname resolution support, #784).

#### `xdg` module
- `XdgDir`, `resolve_path_with_xdg_cache_defaults`,
  `resolve_path_with_xdg_config_defaults`,
  `resolve_path_with_xdg_runtime_defaults` (#854).

#### `status` module
- `Status` trait with blanket `Liveness` / `Readiness` impls;
  `StatusType` enum.

#### `probing` module
- `VitalsProbe`.
