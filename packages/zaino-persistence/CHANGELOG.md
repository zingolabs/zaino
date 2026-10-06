# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. The driven-port seam between index logic and the concrete key-value
  store: one read/write interface — `Backend`, `BackendReader`, `BackendWriter` —
  that the sync engine (writer) and the serving layer (reader) both depend on,
  implemented by backend adapters.
- `Namespace`, `RawKey`, `RawValue`, `WriteOp` — namespaced keys and the batched
  write operations a writer commits atomically, so a partially-applied write can
  never be observed.
- Typed errors — `OpenError`, `CommitError`, `ReadError`, `FlushError` — each
  preserving the backend's own failure as a boxed cause while naming no concrete
  backend type. `CommitError::OutOfSpace` is modelled distinctly, since no retry
  can clear it on its own.
- An `in_memory` backend behind the `testing` feature: a correct, IO-free
  implementation for tests and benchmarks, with a latency-injecting wrapper.
