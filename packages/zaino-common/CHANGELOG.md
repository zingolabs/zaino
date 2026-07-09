# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `ActivationHeights.nu6_3` (serde key `"NU6.3"`) for the NU6.3 network
  upgrade. `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS` currently leaves it `None`
  (inactive); a chain with NU6.3 active needs the height stated explicitly.
- `StorageConfig::database.sync_checkpoint_interval` (seconds, default 120) — max
  wall-clock time spent buffering a bulk-sync write batch before flushing. Under
  the env's `NO_SYNC` mode this also bounds the window of unflushed writes at risk
  on a hard kill / eviction; lower it to shrink that window.
- `StorageConfig::database.accumulator_rebuild_memory_size` (GiB, default 8, new
  `AccumulatorRebuildMemorySize` newtype) — dedicated heap budget for the
  from-genesis txout-set accumulator rebuild's spent set, kept **separate** from
  `sync_write_batch_size` so the two operations cannot inflate each other's peak
  memory.
### Changed
- `crypto::ensure_default_crypto_provider` now installs rustls's
  **aws-lc-rs** provider (was ring) as the process-level default, and the
  crate's rustls features become `aws_lc_rs` + `prefer-post-quantum`
  (ADR-0006). First-install-wins semantics are unchanged.
- **Breaking** — `StorageConfig::database.sync_write_batch_bytes` (raw bytes) is
  renamed to `sync_write_batch_size` and now expressed in **GiB** (new
  `SyncWriteBatchSize` newtype, mirroring `DatabaseSize`); the default is 8 GiB.
  It is now a heap budget for buffered blocks only — the accumulator rebuild uses
  the separate `accumulator_rebuild_memory_size`. Existing TOML configs setting
  `sync_write_batch_bytes` must switch to `sync_write_batch_size` (in GiB).
- **Breaking** — `DatabaseConfig` now uses `#[serde(deny_unknown_fields)]`: an
  unrecognized key under `[storage.database]` (e.g. a stale `sync_write_batch_bytes`)
  is a hard parse error instead of being silently ignored and falling back to the
  default budget — the silent fallback previously OOM-killed nodes.
- The `ActivationHeights` ↔ `ConfiguredActivationHeights` conversions and the
  zebra-network → `ActivationHeights` extraction are now generated from a single
  variant/field pair list (internal refactor; conversion behavior unchanged).
- `logging::init` / `logging::try_init` build their subscriber through one shared
  installer (internal refactor). The `ZAINOLOG_FORMAT` / `ZAINOLOG_COLOR` /
  `RUST_LOG` runtime interface and output formats are unchanged.
### Deprecated
### Removed
- Unused dependencies `thiserror`, `nu-ansi-term`, and `hex` (`hex`'s last
  consumers were the display wrappers removed below). Verified empirically:
  each dependency was deleted in turn and the crate re-checked with
  `cargo check --all-targets`.
- **Breaking** — `Network::zaino_regtest_heights` (unused; regtest heights come
  from `ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS` or an explicit `ActivationHeights`).
- **Breaking** — `logging::DisplayHash` and `logging::DisplayHexStr` (unused).
- **Breaking** — the unused programmatic logging-configuration surface:
  `logging::init_with_config` / `logging::try_init_with_config` and the `LogConfig`
  / `LogFormat` types (including the `LogConfig` builder methods and
  `show_span_events`, which no caller could reach). Logging is configured via the
  `ZAINOLOG_FORMAT` / `ZAINOLOG_COLOR` / `RUST_LOG` environment variables, whose
  behavior is unchanged.
### Fixed

## [0.2.0] - 2026-06-17

### Added
- `StorageConfig::database.sync_write_batch_bytes` (default 4 GiB) — byte budget
  for the finalised-state bulk-sync / migration write batch. Larger batches
  insert the random-keyed `spent` / `txid_location` indexes in bigger sorted
  sweeps (fewer random B-tree faults once the DB exceeds RAM), at the cost of
  more RAM; raise it on large-RAM hosts.
- `ActivationHeights::nu6_2` (serialised as `NU6.2`) and the matching
  `set_nu6_2` builder, configuring the NU6.2 network-upgrade activation height
  so regtest / test networks can activate NU6.2.
### Changed
- **Breaking** — `ActivationHeights` gains a public `nu6_2` field. The struct is
  not `#[non_exhaustive]`, so external struct-literal construction must now
  supply the field (analogous to the `ZainodConfig.donation_address` break in
  0.3.0).
### Deprecated
### Removed
### Fixed

## [0.1.1] - 2026-05-19

### Added
- `logging` module (#888) — initial structured-logging surface for the
  Zaino crates:
  - `LogConfig` and `LogFormat`.
  - `init`, `try_init`, `init_with_config`, `try_init_with_config`
    helpers.
  - `DisplayHash`, `DisplayHexStr` display wrappers.

### Changed
- `LogConfig::default` color auto-detection uses
  `std::io::stderr().is_terminal()` (#1020) — the `atty` crate is no
  longer a dependency. Behavior is unchanged.

## [0.1.0] - 2026-03-25

Initial release on crates.io.
