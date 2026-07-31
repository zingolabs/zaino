# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. `ZebraRpcAdapter` implements every `zaino-source` port that
  JSON-RPC can answer, so it is the one transport always present in a
  deployment; the read-state adapter is an optional accelerator.
- `parse.rs` — `serde_json::Value` → domain types, replacing `zaino-fetch`'s
  inbound half. This is Zaino's external-input validation: a reply that does
  not say what it should is an error rather than a default.
- Error classification helpers, one per class rather than one per method:
  `absent_or_fetch` (height/hash-keyed reads), `invalid_address_or_fetch`
  (address-keyed reads), `call_parsed_optional` (`gettxout`),
  `submission_rejection` (`sendrawtransaction`), `spent_info_rejection`
  (`getspentinfo`).

### Changed
- `getspentinfo` no longer shares `call_parsed_optional` with `gettxout`. It
  has no null answer in the interface, so `-5` becomes the `NotSpent` domain
  rejection and `-32601` becomes `Unsupported` — the latter because zebrad does
  not implement the method at all, and reading that as "unspent" would report
  every output as unspent on a zebrad-backed deployment.

### Deprecated
### Removed
### Fixed
- **A missing object is now told apart from an unreachable validator.** This
  adapter originally constructed no `QueryError::Domain` at all, so a validator
  answering "that block does not exist" — which the ChainIndex sync loop asks
  on every iteration — arrived as an unrecoverable transport fault and
  exhausted the retry ladder against a perfectly healthy node. Every port
  already defined the right error variant; the adapter simply never produced
  one. Found by the live suite.
- Both spellings of the deferred-development-fund value pool are accepted:
  zebrad serialises it `lockbox`, zcashd calls it `deferred`. Previously a
  zebrad reply failed to parse, breaking `getblockchaininfo`.
