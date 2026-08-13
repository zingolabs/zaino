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
- `MAX_RESPONSE_BYTES` (32 MiB) and a chunk-wise capped body reader. Responses
  are deserialized into memory, so an uncapped read let a compromised,
  misconfigured, or impersonated validator exhaust the process's memory with a
  single reply. Bodies are now abandoned part-way rather than buffered, and the
  new `RpcError::ResponseBodyTooLarge` classifies as `FailureMode::Parse` so
  `Resilient` does not re-issue the request and buffer the same body again.
- `RpcClient::call_with_timeout` and `HEAVY_METHOD_TIMEOUT` (120s), for the few
  methods that are inherently heavy on the validator (`getrawmempool verbose`
  walks the whole mempool). The client-wide timeout (30s) stays tight for the
  small, fast RPCs that dominate traffic. The constant must exceed that default
  or the override is inert; a unit test pins the relationship.

### Changed
### Deprecated
### Removed
### Fixed
- `probe_node` returns an error instead of calling `std::process::exit(1)`, as
  its predecessor in `zaino-fetch` did. Exiting the process from a library made
  the startup path untestable and gave an embedding process no say in its own
  shutdown.
