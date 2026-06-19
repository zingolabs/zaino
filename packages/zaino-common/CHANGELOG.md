# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `StorageConfig::database.sync_checkpoint_interval` (seconds, default 300) — max
  wall-clock time spent buffering a bulk-sync write batch before flushing. Raise
  it to make sync less reactive but faster on large-RAM hosts where the memory
  budget is never reached before the interval.
### Changed
- **Breaking** — `StorageConfig::database.sync_write_batch_bytes` (raw bytes) is
  renamed to `sync_write_batch_size` and now expressed in **GiB** (new
  `SyncWriteBatchSize` newtype, mirroring `DatabaseSize`); the default is raised
  from 4 GiB to 32 GiB. This budget now also bounds the per-shard memory of the
  finalised-state txout-set accumulator rebuild. Existing TOML configs setting
  `sync_write_batch_bytes` must switch to `sync_write_batch_size` (in GiB).
### Deprecated
### Removed
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
