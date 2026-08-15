# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added
- **Eight new crates** implementing validator access as a hexagonal port /
  adapter stack (ADR-0008, ADR-0009). Each carries a `usage.md`:
  - `zaino-primitives` — Zaino's domain vocabulary. Depends on `thiserror` and
    nothing else; deliberately no serde.
  - `zaino-source` — the driven ports: 36 single-method traits, one per question
    a consumer can ask, each with its own error type. Plus `QueryError`,
    `FetchError`/`FailureMode`, the `Resilient` retry decorator, and `MockChain`.
  - `zaino-rpc` — JSON-RPC transport only: HTTP, envelope, auth, retry-on-`-1`.
  - `zaino-convert-zebra` — `zebra-chain` → domain conversions, in one place.
  - `zaino-source-zebra-rpc` — the JSON-RPC adapter, plus response parsing.
  - `zaino-source-zebra-readstate` — the read-state adapter.
  - `zaino-source-zebra` — the `ZebraValidator` composite and its routing.
  - `zaino-address` — Zcash address classification, isolating a heavy
    dependency set behind a leaf crate.
- `zaino-serve` now owns **the served JSON schema** in `rpc/jsonrpc/wire/`
  (ADR-0009), with golden serialization tests beside each type.
- **Two more crates for the mempool subsystem** (ADR-0010), replacing the
  `Broadcast`-backed mempool inside `zaino-state`:
  - `zaino-mempool` — the domain types and ports. Reads the validator through
    `zaino-source`, and names no node library at all: entries hold the
    validator's bytes and never parse them.
  - `zaino-mempool-service` — the runtime: the polling core, the read handles,
    and the tip-aware coherence layer.
- Three mempool sourcing ports in `zaino-source` — `GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip` — all of which an adapter
  must route to the same transport as `GetMempoolTxids`.
- `[mempool]` config section in `zainod`, making the mempool memory bound, poll
  cadence and exclude-list caps operator-configurable.

### Changed
- **The mempool no longer stalls across a tip transition.** `getrawmempool`,
  `getmempoolinfo` and `GetMempoolTx` are served from a tip-agnostic set that
  never clears; the old mempool wiped its whole map on every tip change and
  answered as if empty until it had re-fetched every transaction.
- **The reads that place a transaction relative to a tip now refuse to answer
  against a stale snapshot** rather than answering with a consensus branch id
  derived from the wrong height. `get_raw_transaction`, `get_transaction_status`
  and `GetMempoolStream` return a retryable error instead; a caller cannot tell
  a wrong branch id from a right one, but it can retry.
- **`GetTransaction` reports height `0` for an unmined transaction** — the
  lightwalletd sentinel — rather than the chain tip, which claimed the
  transaction was mined at a height it is not in.
- **The validator abstraction is now a set of single-question ports in domain
  vocabulary** rather than a 34-method trait declared in `zebra-chain`,
  `zebra-rpc` and `zaino-fetch` types (ADR-0008). Errors distinguish a domain
  answer from a transport failure, so retry policy is a property of the type;
  capability is structural, so an adapter that cannot answer a question does not
  implement its port; and preference is a routing table rather than a 3,145-line
  enum matched in every method.
- **Breaking** — `zaino-state`'s `ZcashIndexer` returns domain types from all 25
  non-proto methods, including those that previously returned
  `zebra_rpc::methods::*`. `z_getblock` and `getrawtransaction` keep zebra's
  presentation shapes by decision.
- `zaino-state`'s `BlockchainSource` survives as documented **temporary
  scaffolding** with a "do not extend" note, so ChainIndex keeps working while
  the new stack is proven underneath it. It shrinks as each subsystem moves onto
  the real ports.
- Config, RPC surface and gRPC surface are unchanged. This is an internal
  rewire.
- `zaino-state`: `FetchService` and `StateService` are merged into a single
  generic `NodeBackedIndexerService<Source>` (module
  `zaino_state::indexer::node_backed_indexer`; the former `backends` module is
  gone). The validator connection is now selected at runtime rather than by type:
  `NodeBackedIndexerServiceConfig { common, connection }` carries a
  `ValidatorConnectionType` of either `Rpc` (JSON-RPC, formerly `Fetch`) or
  `Direct(DirectConnectionConfig)` (Zebra `ReadStateService`, formerly `State`).
  The per-backend `Fetch/StateServiceConfig`, `Fetch/StateServiceError`, and
  `BackendConfig` types are replaced by `NodeBackedIndexerServiceConfig`,
  `NodeBackedIndexerServiceError`, and `ValidatorConnectionType`.
- **Breaking** — config: `zainod.toml`'s `backend` selector is renamed
  `state` → `direct` and `fetch` → `rpc`. The legacy `"state"` / `"fetch"`
  values are still accepted as aliases, so existing config files keep working.
- `zaino-state`: the `ChainIndex` trait is split into `ChainIndex` (the
  wallet-essential core: chain/tx/address/mempool access) and a
  `ChainIndexRpcExt: ChainIndex` extension (compact-block serving, subtree
  roots, and the block-explorer / mining / node-passthrough RPCs). The split is
  a provisional first pass to be refined into finer capability traits later.
- `zaino-state`: all remaining backend-split RPC functionality has moved out of
  the `FetchService` (`JsonRpSeeConnector`) and `StateService`
  (`ReadStateService`) backends and into `BlockchainSource` /
  `ChainIndex`. Both backends now resolve every fetch through their `ChainIndex`
  indexer — building responses from Zaino's own indexed state where possible and
  delegating to the `ValidatorConnector` (`BlockchainSource`) only for
  validator-only or passthrough data. Validator connection/syncer spawning also
  moves into `ValidatorConnector::spawn_fetch` / `spawn_state`, so each
  service/subscriber now holds only `{ indexer, data, config }`. This readies
  the two backends for their eventual merge into a single
  `ValidatorBackedIndexerService`. No behaviour change.
- TLS: zaino now installs rustls's **aws-lc-rs** CryptoProvider as its
  preferred process-level default (was ring) and enables rustls's
  `prefer-post-quantum` feature, so the X25519MLKEM768 hybrid key exchange
  leads zaino's outbound handshakes (ADR-0006). Installation remains
  first-install-wins: an embedder that installs a provider before zaino
  keeps its choice.

### Deprecated
- Classical TLS key exchange (X25519, SECP256R1, SECP384R1) is deprecated:
  still offered and accepted for wallet compatibility, slated for refusal
  once major wallet stacks negotiate hybrid key exchange (ADR-0006).
- **Breaking** — config: `storage.database.sync_write_batch_bytes` (bytes) is
  renamed to `sync_write_batch_size` and given in **GiB** (default raised from
  4 GiB to 32 GiB); this budget now also bounds the txout-set accumulator
  rebuild's per-shard memory. New `storage.database.sync_checkpoint_interval`
  (seconds, default 300) makes the bulk-sync flush interval configurable (was a
  fixed 60s).

### Removed
- **`zaino-fetch` is deleted from the workspace.** It was dual-purpose —
  deserializing validator replies *and* serializing Zaino's own JSON-RPC
  replies — which is why replacing its transport did not remove it. The three
  roles now have three owners: `zaino-rpc` (transport),
  `zaino-source-zebra-rpc` (inbound parsing), `zaino-serve`'s wire module
  (outbound serialization). Its legacy protocol parser moved to
  `live-tests/zaino-testutils` as a test-only module, kept deliberately
  independent of the parser under test so the test vectors remain a real
  oracle.
- The `zcashd_support` feature declaration on `zaino-state`, which gated
  nothing once the zcashd-shaped types moved to `zaino-serve`. The feature and
  its behaviour are unchanged; `zaino-serve` is now the only crate where it
  gates code (ADR-0001, ADR-0005).

### Fixed
- JSON-RPC responses are read against a 32 MiB cap, chunk-wise. Every response
  is deserialized into memory, so an uncapped read let a compromised,
  misconfigured or impersonated validator exhaust Zaino's memory with one reply.
- Every client-controllable mempool input is bounded: the exclude list's count
  and per-suffix length, and both mempool listings on their declared entry count
  — the latter before any entry is decoded, so an oversized listing cannot drive
  a million raw-transaction fetches.
- The mempool's per-transaction entry height is sourced from the validator
  rather than derived locally. The two disagree exactly when the chain moves
  under a transaction, which is the case that matters.
- Zaino no longer OOM-crashes during the txout-set accumulator rebuild when it
  reaches mainnet chain tip on memory-constrained hosts; the rebuild auto-shards
  its in-memory spent set to fit the configured `sync_write_batch_size` budget.
- **A missing object is told apart from an unreachable validator.** The JSON-RPC
  adapter reported "no block at that height" — which the ChainIndex sync loop
  asks on every iteration — as an unrecoverable transport fault, exhausting the
  retry ladder against a perfectly healthy node.
- **zcashd error-code recovery was silently inert.** `zaino-serve` recovered
  codes by downcasting for a `zaino-fetch` type the new stack never constructs,
  so every code reached the client as a generic internal error.
- `getblockdeltas` is served on zebrad-backed deployments. zebrad does not
  implement the method, and the read-state derivation that answered it had been
  omitted on the mistaken reasoning that the validator already provided it.
- `getblockchaininfo` and `z_getblock` work against zebra 6.0, which serialises
  the deferred-development-fund value pool as `lockbox` where zcashd calls it
  `deferred`.
- `getspentinfo` reports zcashd's own `-5` / `Unable to get spent info`, and
  reports `-32601` rather than a not-found when the backing validator is zebrad
  (which does not implement it). Neither Zaino nor its predecessor served the
  `-5`. **Zaino still does not answer `getspentinfo` from its own index** — a
  documented gap, not a fix, and one that matters because zebrad will never
  implement the method. See `zaino-source`'s `GetSpentInfo` for what would be
  needed.
- Network upgrade names no longer differ between the two transports (`Nu5` vs
  `NU5`).
- The mempool stream parses each transaction once rather than twice, removing an
  `.unwrap()` on the same path.

## [0.4.1] - 2026-06-18
- Bump zaino-proto 0.1.2 → 0.1.3 and zainod 0.4.0 → 0.4.1 to work around
  a yanked 0.1.2 slot on crates.io. No code changes.

## [0.4.0] - 2026-06-17
- NU6.2 network upgrade is now supported: activation-height configuration
  (`zaino-common`) and Zebra RPC response parsing (`zaino-fetch`) recognise
  NU6.2.
- [943] Zallet regtest fixes
- [1065] Move functionality to BlockChainSource: t-address rpcs
- `gettxoutsetinfo` is now served indexer-side. Both `FetchService` and
  `StateService` compute the response from Zaino's own UTXO-set accumulator
  (finalised state + non-finalised state) instead of forwarding to the backing
  validator.

### Added
- `storage.database.sync_write_batch_bytes` config (default 4 GiB) tunes the
  finalised-state bulk-sync / migration write-batch size.
- `zainod` gains an `allow_unencrypted_public_json_rpc_bind` build feature that
  lifts the new private-only JSON-RPC bind restriction for trusted
  private-network deployments (logs a `WARN` on startup when enabled).
- `zaino-state::chain_index::source::BlockchainSource` and
  `zaino-state::chain_index::ChainIndex` now expose transparent-address query
  methods for deltas, balances, txids, and UTXOs.
- `ChainIndex::get_tx_out_set_info` — combines the finalised
  `FinalisedTxOutSetInfoAccumulator` with the non-finalised state to produce
  the full `GetTxOutSetInfoResponse`.
- Optional ("ephemeral") finalised state: `zainod` gains an
  `ephemeral_finalised_state` config option (default `false`) that runs Zaino
  without a persistent finalised-state database, serving finalised reads from
  the backing validator via an ephemeral passthrough.
- `ChainIndex::get_outpoint_spenders` — resolves, for each transparent
  outpoint, the txid that spent it on the best chain (or `None` if unspent),
  with a `ChainScope` selecting finalised-only or full-chain search.
### Changed
- Finalised-state sync and the v1.1.0 -> v1.2.0 migration are substantially
  faster on large/mainnet caches. The txout-set accumulator is built in bulk at
  the tip instead of per block (removing an unbounded fan-out of random reads),
  block validation is off the write path, and the random-keyed `spent` /
  `txid_location` indexes are written in sorted batches — together removing the
  random-fault stall around sandblast height. See the `zaino-state` changelog for
  details; tune the write-batch size with `storage.database.sync_write_batch_bytes`.
- Finalised-state sync and version migrations are now background, non-blocking
  operations: large syncs and migrations run while an ephemeral passthrough
  serves finalised reads, so startup and serving are no longer blocked on
  persistence. Internally the finalised-state facade `ZainoDB` was renamed
  `FinalisedState` and its backing `DbBackend`/`db` module became
  `FinalisedSource`/`finalised_source` (now covering an ephemeral passthrough,
  not only databases). Bumps the finalised DB version to v1.2.1 (metadata-only).
- The `zainod` JSON-RPC server now refuses to bind to public or unspecified
  (`0.0.0.0` / `::`) addresses by default; `check_config` enforces the same
  private/loopback rule already applied to gRPC. The unencrypted JSON-RPC
  interface is intended for loopback or trusted private networks only (Z-02 /
  Zellic #48480).
- `get_address_utxos` now bounds the number of addresses fanned out per request,
  preventing an unbounded multi-address query from amplifying backend load
  (#974).
- Integration tests now use `corez`, with Zcash, Zebra, and Zingo dependencies
  updated to releases and companion branches that no longer depend on the
  yanked `core2` crate.
- Integration tests now follow the companion Zingo corez migration branches and
  use `zcash_client_backend` 0.22, with deprecated nullifier-range client calls
  allowed locally until they are replaced.
- `JsonRpSeeConnector::get_tree_state` now returns a `GetTreestateResponse`
  whose `sapling` and `orchard` fields are optional. In regtest mode, these
  fields may be omitted when the corresponding network upgrade activation
  height is not configured.
### Removed
### Deprecated
### Fixed
- Finalised-state DB v1.2.0 migration no longer appears to hang on large caches.
  A reverse transaction-id index (`txid_location`) makes previous-output
  resolution an O(log n) lookup instead of a full table scan, removing a
  near-quadratic cost in both the migration backfill and the clean-sync write
  path. The v1.1.0 -> v1.2.0 migration is now a re-entrant two-stage backfill
  with progress logging, and caches built by 0.4.0-alpha.1 self-heal on open.
- Nullifiers-only compact blocks (`compact_block_to_nullifiers`) no longer leak
  transparent `vin` / `vout`, restoring lightwalletd compact-block parity
  (#1067).

## [0.3.1] - 2026-05-25

Re-release of 0.3.0 to publish the `zainod` binary's container image under the
new `zainod` Docker Hub repository alongside the legacy `zaino` repository
(#1133, #1134). No functional changes to any crate since 0.3.0.

## [0.3.0] - 2026-05-22

### Added
- Transparent-address queries on the `zaino-state` `ChainIndex` trait —
  `get_address_balance`, `get_address_deltas`, `get_address_txids`,
  `get_address_utxos` (#1065) — plus block lookups (#1000) and subtree-root
  reporting (#853).
- `zaino-state` shared `CommonBackendConfig` payload carrying an
  `indexer_version` field, and a `DonationAddress` type (#1008).
- `zainodlib::config::ZainodConfig` gains an optional `donation_address` field;
  0.2.0 TOML configs continue to load (the field defaults to absent) (#1008).
- `z_validateaddress` JSON-RPC passthrough across `zaino-fetch` and the
  `zaino-serve` `ZcashIndexerRpc` trait, shipped pre-deprecated (#389).
- `zaino-common` `logging` module — the initial structured-logging surface for
  the Zaino crates (#888).
- `zaino-proto` Cargo features `heavy` (default) and `grpc_proxy_server`; build
  wiring moved to `tonic-prost` / `tonic-prost-build` 0.14.

### Changed
- **Breaking** — the `ChainIndex` (`zaino-state`) and `ZcashIndexerRpc`
  (`zaino-serve`) traits gain required methods with no default body, so
  downstream implementers must add them; adding `donation_address` to
  `ZainodConfig` is likewise breaking for struct-literal construction (#1008).
- `LightdInfo.version` now reports the running `zainod` binary version rather
  than the `zaino-state` library version (#1061).

### Fixed
- Restart path no longer crashes when the validator's readiness signal arrives
  before the indexer's status is observed (#962).

## [0.2.0] - 2026-03-25
- [808] Adopt lightclient-protocol v0.4.0

### Added
### Changed
- zaino-proto now references v0.4.0 files
- `zaino_fetch::jsonrpsee::response::ErrorsTimestamp` no longer supports a String
  variant.
### Removed

### Deprecated
- `zaino-fetch::chain:to_compact` in favor of `to_compact_tx` which takes an
  optional height and a `PoolTypeFilter` (see zaino-proto changes)
- `zaino_fetch::FullTransaction::to_compact` deprecated in favor of `to_compact_tx` which includes
  an optional for index to explicitly specify that the transaction is in the mempool and has no
  index and `Vec<PoolType>` to filter pool types according to the transparent data changes of
  lightclient-protocol v0.4.0
- `zaino_fetch::chain::Block::to_compact` deprecated in favor of `to_compact_block` allowing callers
  to specify `PoolTypeFilter` to filter pools that are included into the compact block according to
  lightclient-protocol v0.4.0
- `zaino_fetch::chain::Transaction::to_compact` deprecated in favor of `to_compact_tx` allowing callers
  to specify `PoolTypFilter` to filter pools that are included into the compact transaction according
  to lightclient-protocol v0.4.0.

---

This file tracks **Zaino workspace** releases only. Two related histories live
elsewhere:

- The lightwallet / `walletrpc` **protocol** changelog (proto-definition version
  history, v0.1.0 → v0.4.0) is at
  `packages/zaino-proto/lightwallet-protocol/CHANGELOG.md`.
- The `zaino-proto` **Rust crate** changelog is at
  `packages/zaino-proto/CHANGELOG.md`.
