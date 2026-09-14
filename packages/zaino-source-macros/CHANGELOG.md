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

## [0.1.0] - 2026-08-28

### Added
- New crate. The `#[resilient_port]` proc-macro: applied to a `OneShot*` port
  trait in `zaino-source`, it derives the canonical resilient twin trait and
  the `ValidatorClient<V>` blanket impl, rewriting `QueryError<E>` ->
  `SourceError<E>` so the twin's signature is never restated.
### Changed
### Deprecated
### Removed
### Fixed
