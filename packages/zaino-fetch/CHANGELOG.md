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

## [0.2.0] - 2026-06-17

### Added
- `JsonRpSeeConnector::get_tx_out_set_info` — JSON-RPC client method for the
  upstream `gettxoutsetinfo` call.
- `jsonrpsee::response::GetTxOutSetInfoResponse` (`Info` | `Empty` untagged
  enum), `GetTxOutSetInfo` and `EmptyTxOutSetInfo` types covering both the
  populated and stats-collection-failed shapes returned by zcashd.
- `JsonRpSeeConnector::get_chain_tips`, `get_tx_out`, and `get_spent_info` —
  JSON-RPC client methods for the upstream `getchaintips`, `gettxout`, and
  `getspentinfo` calls.
- `jsonrpsee::response::{GetChainTipsResponse, ChainTip, ChainTipStatus}` —
  types modelling the `getchaintips` response.
### Changed
- NU6.2 network-upgrade variant added to Zebra RPC response parsing, so
  activation-height responses that include `NU6.2` are recognised.
### Deprecated
### Removed
### Fixed

## [0.1.1] - 2026-05-19

### Added

- New JSON-RPC passthrough method `JsonRpSeeConnector::z_validate_address`
  under `jsonrpsee::response::z_validate_address`, with response and
  error types `ZValidateAddressResponse`, `KnownZValidateAddress`,
  `ValidZValidateAddress`, `InvalidZValidateAddress`,
  `ZValidateAddressType`, `ZValidateAddressError`, the supporting
  `AddressData` / `CommonFields` types, and the `DEPRECATION_NOTICE`
  constant. Shipped pre-deprecated; emits
  `tracing::warn!(DEPRECATION_NOTICE)` on every call and exists only
  for zcashd `z_validateaddress` bugwards compatibility (#389).

## [0.1.0] - 2026-03-25

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.
