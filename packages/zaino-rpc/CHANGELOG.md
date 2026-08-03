# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. The JSON-RPC transport, replacing `zaino-fetch`'s
  `JsonRpSeeConnector`: HTTP, the request/response envelope, authentication,
  and retry-on-`-1`.
- `RpcClient` / `RpcClientConfig`. `call()` returns a raw `serde_json::Value` —
  response parsing is the adapter's job, which is what lets one client serve
  both the production adapter and the live tests' independent oracle.
- `RpcError`, convertible into `zaino_source::FetchError` so the source layer's
  `FailureMode` classification works end to end.
- `probe_node` / `auth_from_parts` — startup validator handshake, 6 attempts
  3 seconds apart.
- `metric_names` — the outbound RPC metric names, moved from `zainod` because
  this crate is what emits them. Registration and descriptions stay with the
  daemon.

### Changed
### Deprecated
### Removed
### Fixed
- `probe_node` returns an error instead of calling `std::process::exit(1)`, as
  its predecessor in `zaino-fetch` did. Exiting the process from a library made
  the startup path untestable and gave an embedding process no say in its own
  shutdown.
