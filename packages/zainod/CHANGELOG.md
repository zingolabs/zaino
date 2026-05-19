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

This release covers all main-branch development since the previous
stable tag `v0.1.2-zr4` (April 2025).

### Added

- **CLI module** — `zainodlib::cli::{Cli, Command}` plus
  `default_config_path()`, surfacing the binary's argument parsing as
  library API (#854).
- **Top-level async entrypoint** — `zainodlib::run(config_path: PathBuf)`
  drives the indexer loop with restart support; the binary's `main.rs`
  is now a thin wrapper around it (#854).
- **Backend-pluggable `Indexer<Service>`** — `Indexer` becomes generic
  over `Service: ZcashService + LightWalletService`, with the new
  `start_indexer` and `spawn_indexer` async free functions as the
  preferred entry points (#311).
- **TOML config generator** — `GENERATED_CONFIG_HEADER` constant plus
  `generate_default_config()` emit a documented default config
  (#854).
- **Env-aware config loading** — `load_config_with_env()` layered on
  top of `load_config()` (#469, part of the `figment` → `config-rs`
  migration).
- **Optional `donation_address`** — `ZainodConfig.donation_address:
  Option<DonationAddress>`, validated against `zcash_address` when
  present (#1008).
- **Path helpers** — `default_ephemeral_cookie_path()` and
  `default_zebra_db_path()` (#297, with XDG defaults refined in #854).
- **Error conversions** — `From<StateServiceError> for IndexerError`
  and `From<FetchServiceError> for IndexerError`, so service errors
  propagate cleanly with `?`.

### Changed

- **Breaking — `IndexerConfig` renamed to `ZainodConfig`.** The
  `Default` impl moves with it. Any consumer of zainodlib's config
  type must rename their import (#571).
- **Breaking — `Indexer` now requires a generic parameter.**
  Signature is now `Indexer<Service: ZcashService +
  LightWalletService>`; the previous parameterless `Indexer` is no
  longer constructible directly. Use `spawn_indexer` / `start_indexer`
  for the typical flow (#311).
- `load_config()` accepts `&std::path::Path` instead of
  `&std::path::PathBuf` (source-compatible — `PathBuf` derefs to
  `Path`) — fallout from #469.
- `LightdInfo.version` now reports the running `zainod` binary
  version rather than the `zaino-state` library version (#1061). The
  binary's `env!("CARGO_PKG_VERSION")` is threaded through
  `StateServiceConfig` / `FetchServiceConfig` via a new
  `indexer_version` field on the shared `CommonBackendConfig` payload.

### Fixed

- Restart path no longer crashes early when the validator's readiness
  signal arrives before the indexer's status is observed (#962).
