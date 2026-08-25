# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. How a Zaino component reports whether it is working, moved out of
  `zaino-common` — `StatusType`, `Status`, `NamedAtomicStatus`, and the
  `Liveness` / `Readiness` / `VitalsProbe` probing traits.
- Status is the one thing every subsystem has, including those whose whole
  purpose is to depend on as little as possible. Reporting one previously cost a
  dependency on the validator config, the logging stack, TLS and `zebra-chain`.
  The dependency list here is `tracing` and nothing else, and the crate stays
  that way: this is vocabulary, not machinery.
- Probing moved with status rather than after it: `Liveness` and `Readiness` are
  blanket impls over `Status`, so the two cannot be separated without giving up
  the blanket impls.
- `NamedAtomicStatus::apply` replaces the status with `f(current)` in one
  compare-and-swap loop, for transitions that depend on the current status; a
  `load` followed by a guarded `store` leaves a window in which a concurrent
  transition is silently overwritten, and `apply` closes it.

### Changed
### Deprecated
### Removed
### Fixed
