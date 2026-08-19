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

## [0.8.0] - 2026-08-14

### Added
- `zaino.mempool.coherence_frozen_seconds` metric description: how long
  tip-coherent mempool reads have been frozen. Brief spikes are normal tip
  transitions; a sustained non-zero value means the validator tip and Zaino's
  have stopped agreeing and those reads are unavailable.
- `[mempool]` config section — `max_cost_bytes` (default 128 MiB, the mempool
  memory backstop), `poll_interval_ms` (default 500), `metadata_min_interval_ms`
  (defaults to the poll interval; raising it trades mempool latency for validator
  load) and `max_exclude_count` (default 1024). Every field is optional and an
  absent section keeps the built-in bounds, so existing config files are
  unaffected. This makes the mempool capacity bound operator-configurable.

  `poll_interval_ms = 0` is rejected by `check_config` with a named error. It is
  not a slow mempool but a crash: the poll and coherence loops both build a
  `tokio::time::interval` from it, and a zero period aborts at startup. The
  operator sees a configuration error instead. `metadata_min_interval_ms = 0` is
  deliberately still accepted — it is a `>=` floor, so zero means "no coalescing
  beyond the poll cadence".
### Changed
- The daemon builds on the new source stack (`zaino-source-zebra`) via
  `zaino-state`. No configuration change: the `[validator] connection` selector
  (`rpc` / `direct`) means the same thing, and now chooses whether the composite
  is constructed with a read-state adapter alongside its RPC one.
### Deprecated
### Removed
- The outbound RPC metric *names* moved to `zaino-rpc`, which is the crate that
  emits them. Registration and the metric descriptions stay here, so the
  exported metrics are unchanged.
- `zcashd_support` no longer forwards to `zaino-state`, which gates nothing
  under it; it forwards to `zaino-serve` alone.
### Fixed
- The startup validator probe returns an error instead of calling
  `std::process::exit(1)` from inside a library, so the daemon controls its own
  shutdown path on an unreachable validator.

## [0.7.0] - 2026-08-04

### Added
### Changed
- Bumped the bundled crate dependencies: `zaino-proto` 0.3.0, `zaino-state`
  0.6.0, `zaino-fetch` 0.4.1, and `zaino-serve` 0.5.1.
- CI now gates on duplicate Rust logic (the workbench `check-code-duplication`
  check).
- ADR 0007 records the block-persistence row-set boundary.
### Deprecated
### Removed
### Fixed

## [0.6.0] - 2026-07-13

### Added
- Ironwood (NU6.3) / V6 transaction support, end to end through the
  workspace crates: V6 parsing and ironwood extraction (`zaino-fetch`),
  `ironwoodActions` in served compact blocks (`zaino-proto`, on by
  default), and ironwood treestate roots in the chain index
  (`zaino-state`).
### Changed
### Deprecated
### Removed
### Fixed

## [0.5.0] - 2026-07-02

### Added
- `[storage.database]` config gains `sync_checkpoint_interval` (seconds, default
  120) — the bulk-sync write-batch flush interval, which also bounds the window of
  unflushed (`NO_SYNC`) writes at risk on a hard kill / eviction.
- `[storage.database]` config gains `accumulator_rebuild_memory_size` (GiB,
  default 8) — a dedicated heap budget for the txout-set accumulator rebuild,
  separate from `sync_write_batch_size`.
### Changed
- **Breaking** — `[storage.database] sync_write_batch_bytes` (bytes) is renamed
  to `sync_write_batch_size` and is now given in **GiB** (default 8). It now
  budgets only the bulk-sync block buffer; the accumulator rebuild uses the new
  `accumulator_rebuild_memory_size`. See the `zaino-common` changelog.
- **Breaking** — unknown keys under `[storage.database]` now fail config parsing
  loudly (e.g. a stale `sync_write_batch_bytes`) instead of being silently ignored
  and falling back to the default budget.
### Deprecated
### Removed
### Fixed
- Zaino no longer silently falls back to a large default write/rebuild budget when
  an old `[storage.database]` key is present — the silent fallback to the (former
  32 GiB) default is what OOM-killed nodes at mainnet chain tip (e.g. a 16 GiB
  pod), and the kill, under `NO_SYNC`, then corrupted the on-disk database.

## [0.4.1] - 2026-06-18

### Changed
- Bump zaino-proto dependency from 0.1.2 to 0.1.3 (crates.io publish fix;
  no code changes).

## [0.4.0] - 2026-06-17

### Added
- New `allow_unencrypted_public_json_rpc_bind` build feature. The JSON-RPC
  interface has no transport encryption and is now restricted to private /
  loopback bind addresses by default; this feature lifts that restriction for
  deployments on trusted private networks where encryption is handled
  externally. It logs a `WARN` on startup when enabled.
- New `ephemeral_finalised_state` config option (default `false`). When `true`,
  Zaino runs without a persistent finalised-state database: finalised reads are
  served directly from the backing validator via an ephemeral passthrough. Useful
  for disk-constrained or disposable deployments.
### Changed
### Deprecated
### Removed
### Fixed
- `check_config` now rejects JSON-RPC bind addresses that are not private or
  loopback (matching the existing gRPC enforcement). Previously no bind-scope
  check was applied to the JSON-RPC server, so an operator could expose the
  unencrypted interface on a public address with no warning (Z-02 /
  Zellic #48480).
- Upgrading a cached database to finalised-state DB v1.2.0 no longer appears to
  hang on large (e.g. mainnet) caches. The v1.1.0 -> v1.2.0 migration now builds
  a reverse transaction-id index so previous-output resolution is fast, runs as
  a re-entrant two-stage backfill, and logs progress. Caches built by
  0.4.0-alpha.1 are detected and repaired automatically on startup.

## [0.3.1] - 2026-05-22

Re-release of 0.3.0 to publish the binary's container image under the
new `zainod` Docker Hub repository alongside the legacy `zaino`
repository (#1133). No functional changes to the binary or
`zainodlib` API since 0.3.0.

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
