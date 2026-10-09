# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. Centralised `tracing` setup as its own cross-cutting layer,
  depending only on the tracing stack, so a consumer wanting structured logs need
  not pull a heavier "common" grab-bag to initialise them.
- `init` / `try_init` — install the global subscriber and the panic hook,
  configured from the environment (`ZAINOLOG_FORMAT`, `ZAINOLOG_COLOR`,
  `ZAINOLOG_LOCATION`, `RUST_LOG`). `try_init` is idempotent for tests.
- A panic hook that logs each panic as a structured `error` event at its origin
  (thread, location, message) through the same sink as every other error, so a
  panic on a worker thread is not lost as a distant `JoinError`. It forces the
  `panic` target on so no filter drops the event, and chains to the existing hook
  so the default backtrace is unchanged.
