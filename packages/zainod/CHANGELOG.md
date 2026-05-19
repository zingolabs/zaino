# Changelog
All notable changes to this binary and library (`zainodlib`) will be
documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this crate adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.3.0] - 2026-05-19

### Added

- **Breaking** — `zainodlib::config::ZainodConfig` gains a new
  optional field `donation_address: Option<DonationAddress>` (#1008).
  Adding a public field to a public struct without
  `#[non_exhaustive]` is a breaking change under
  [RFC 2008](https://rust-lang.github.io/rfcs/2008-non-exhaustive.html)
  (consumers that construct `ZainodConfig` via a struct literal must
  add the new field). TOML configs from 0.2.0 continue to load — the
  field defaults to `None` when absent.

### Changed

- `LightdInfo.version` now reports the running `zainod` binary
  version rather than the `zaino-state` library version (#1061). The
  binary's `env!("CARGO_PKG_VERSION")` is threaded through
  `StateServiceConfig` / `FetchServiceConfig` via the new
  `indexer_version` field on the shared `CommonBackendConfig`
  payload introduced in `zaino-state` 0.2.0.

### Fixed

- Restart path no longer crashes early when the validator's readiness
  signal arrives before the indexer's status is observed (#962).

## [0.2.0] - 2026-03-26

Initial post-yank release on crates.io. Previous `v0.1.2` (Aug 2025)
was yanked.

Contents include the `zainodlib::cli` module (`Cli`, `Command`,
`default_config_path`), the top-level `run(config_path)` async
entrypoint, the `Indexer<Service: ZcashService + LightWalletService>`
generic type with `start_indexer` / `spawn_indexer` free functions,
the `ZainodConfig` (renamed from `IndexerConfig`) loaded via
`config-rs`, `generate_default_config()` + `GENERATED_CONFIG_HEADER`,
and `load_config_with_env`.
