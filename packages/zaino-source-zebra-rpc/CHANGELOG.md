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
  (`getspentinfo`), `mempool_unavailable_or_fetch` (both `getrawmempool`
  listings, mapping `-32601` to the port's `Unavailable`).
- Impls of the three mempool sourcing ports: `GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip`. The verbose listing names
  its own bound — `zaino_rpc::HEAVY_METHOD_TIMEOUT`, passed to
  `call_parsed_classified` — because the validator answers it by walking its
  whole mempool, so the client-wide timeout would read a busy validator as a
  hard error. Timeout is a per-method value rather than a preset: it composes
  with any error classification instead of multiplying against it.
- `MAX_MEMPOOL_LISTING_ENTRIES` (1,000,000) and `parse_mempool_txids`. Both
  mempool listings are now capped on their declared entry count *before* any
  entry is decoded. `zaino_rpc::MAX_RESPONSE_BYTES` alone bounds the response
  bytes but would still admit several hundred thousand txids, each of which a
  consumer turns into a raw-transaction fetch. A ZIP-401-bounded validator
  cannot approach this cap, so it only trips on one that is compromised,
  misconfigured, or impersonated.

### Changed
- `GetMempoolSourceTip` reads `getblockchaininfo` rather than the
  `getbestblockhash` + `getblock` pair `GetChainTip` uses: one round trip
  answers hash and height together, where reading them separately would let a
  block land between the two calls and hand the consumer a tip that never
  existed.
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
