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

## [0.1.1] - 2026-05-19

### Added

New JSON-RPC passthrough methods on `JsonRpSeeConnector`, each with at
least one response type (and where relevant a typed error) under
`jsonrpsee::response`:

- `z_validate_address` → `ZValidateAddressResponse`,
  `KnownZValidateAddress`, `ValidZValidateAddress`,
  `InvalidZValidateAddress`, `ZValidateAddressType`,
  `ZValidateAddressError`, and the `DEPRECATION_NOTICE` constant.
  Shipped pre-deprecated; emits `tracing::warn!(DEPRECATION_NOTICE)` on
  every call and exists only for zcashd `z_validateaddress` bugwards
  compatibility (#389).
- `get_tx_out` → `GetTxOutResponse` (#1085).
- `get_chain_tips` → `GetChainTipsResponse`, `ChainTip`,
  `ChainTipStatus` (#1092).
- `get_spent_info` → `GetSpentInfoResponse`, `GetSpentInfoRequest`,
  `GetSpentInfoError` (#1093).

Other supporting additions to `jsonrpsee::response`:
- `AddressData`.
- `CommonFields` re-export.

## [0.1.0] - 2026-03-25

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.
