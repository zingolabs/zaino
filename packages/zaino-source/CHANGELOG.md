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

## [0.2.0] - 2026-08-28

### Added
- Two port layers. Each question is both a single-attempt `OneShot*` port
  (returning `QueryError`, implemented by adapters) and a canonical resilient
  port under the unqualified name (`GetBlock`, `GetChainTip`, … returning
  `SourceError`, bound by consumers). The resilient port is the default name
  because resilience is the default; reaching the single-attempt contract means
  naming the awkward `OneShot*`. The resilient ports are sealed, so only
  `ValidatorClient` implements them: a value satisfying a canonical port has
  provably been through the retry ladder.
- `#[resilient_port]` (in the new `zaino-source-macros` crate) — an attribute
  on a `OneShot*` trait that derives its resilient twin and the
  `ValidatorClient<V>` blanket impl, rewriting `QueryError<E>` ->
  `SourceError<E>`. The twin's signature is never restated and the
  retry/translation lives once in `ValidatorClient::with_retry`.
### Changed
- Every one-shot query port is renamed `GetX` -> `OneShotGetX`, freeing the
  canonical names for the resilient twins.
- `Resilient<V>` is renamed `ValidatorClient<V>`. It now implements the
  canonical resilient ports rather than carrying its own inherent methods, and
  forwards `SubscribeBlocks` / `SubscribeChainTip` unchanged.
  `SendRawTransaction` gets no resilient port — retrying a non-idempotent send
  risks a double-submit.
### Deprecated
### Removed
### Fixed

## [0.1.0] - 2026-08-14

### Added
- New crate. The driven ports for validator access: 36 single-method traits,
  one per question a consumer can ask about the chain, declared in
  `zaino-primitives` vocabulary with a per-question error type.
- `QueryError<E>` — separates `Domain(E)` (the validator answered, and this is
  the answer) from `Fetch(FetchError)` (the transport failed). The distinction
  is load-bearing: `Domain` is returned immediately, `Fetch` is retried.
- `FetchError` with a machine-readable `FailureMode`
  (`Connection | Timeout | HttpStatus(u16) | RpcError(i64) | Parse | Auth`),
  so retry policy is a function of the type and a zcashd legacy code can
  survive from the validator to the served response.
- `Resilient<V>` / `RetryPolicy` — retry with exponential backoff, replacing
  the hand-rolled consecutive-failure ladder in the ChainIndex sync loop.
  Deliberately does *not* implement the port traits: it has its own methods
  returning `SourceError`, so a consumer cannot be handed a bare adapter by
  mistake.
- `SourceLifecycle` (shutdown), `SubscribeChainTip` / `SubscribeBlocks`, and
  `PolledChainTip` (built and tested; not yet wired to a consumer).
- `mock::MockChain` behind the `testing` feature — an in-memory chain with
  failure injection, superseding `force_requests_against_source_to_fail`.
- Three mempool sourcing ports, for a consumer reconstructing a mempool rather
  than merely listing one:
  - `GetMempoolMetadata` (+ `MempoolTxMeta`) — `getrawmempool verbose`. Carries
    each transaction's *validator-reported* tip-at-entry height, a protocol
    field a consumer must not derive locally. Kept apart from `GetMempoolTxids`
    because it is a whole-mempool walk, so a consumer can poll the cheap listing
    and reach for this only on a diff.
  - `GetRawMempoolTransaction` — `getrawtransaction(txid, 0)`, with `NotFound`
    as a *domain* answer: a transaction leaving the mempool between listing and
    fetch is the normal race, not a failure.
  - `GetMempoolSourceTip` — the tip of the source that serves the mempool.
    Deliberately not `GetChainTip`, which implementations may route to whichever
    transport is fastest. A consumer tags its published set with this tip and
    later compares the tag against the chain; the comparison is only sound if
    the tag and the set came from one source.

### Changed
- Only `-1` (work queue full) and `-28` (in warmup) are retryable RPC codes.
  Every other code is the validator's considered reply, and asking again
  produces the same one.
- `GetSpentInfo` returns `Result<SpentInfo, _>` rather than
  `Result<Option<SpentInfo>, _>`, with domain rejections `NotSpent` (zcashd's
  `-5`) and `Unsupported` (`-32601`, which is what zebrad answers because it
  does not implement `getspentinfo`). The `Option` shape belonged to its
  neighbour `GetTxOut`, where an absent output is a successful query returning
  JSON `null`; `getspentinfo` has no null answer, so modelling absence as
  `None` forced every consumer to invent an error code on the way out.
- `GetMempoolTxidsError::Unavailable` and `GetMempoolMetadataError::Unavailable`
  now name one condition — *this validator does not expose a mempool* — and are
  actually produced, from `-32601` on `getrawmempool`. They were declared but
  unreachable, because both methods ran through a classifier that mapped every
  failure to `Fetch`. A consumer can now tell "stop asking, this node has no
  mempool" from "the request failed, try again", which is the distinction the
  variants existed to draw.

### Deprecated
### Removed
- `GetSpentInfoError::IndexUnavailable` — declared, documented, and never
  constructed anywhere. Replaced by `NotSpent` / `Unsupported`, which are
  produced.
- `GetMempoolSourceTipError` — the whole type. Its one variant (`NotReady`) was
  unproducible *by construction*, not by oversight: the port's single-source
  rule requires the tip to come from whichever transport serves the mempool,
  which is JSON-RPC, and that answer either carries a tip or fails at the
  transport level. `get_mempool_source_tip` is now typed
  `QueryError<Infallible>`, which says exactly that.

  Contrast `GetChainTipError::NotReady`, which stays: `GetChainTip` may be
  answered from the state database, and the ReadState adapter genuinely
  observes "no tip yet" as an answer. A domain variant one transport cannot see
  but another can is correct; one *no* transport can produce is not.

### Fixed
- `mock` is gated `#[cfg(any(test, feature = "testing"))]` rather than on the
  feature alone. Nothing in the workspace enables `testing`, so the module and
  its 13 tests were never compiled — a gap invisible to `cargo nextest`,
  because a module that is not compiled reports no failures. It was also hiding
  a compile error that only `--all-features` could reach.
