# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added
- `zainod verify --config <path> [--rehash]`: an offline, read-only verifier for
  every enabled index, safe to run beside a live daemon (no writes, no
  directory creation, no lock, no mmap; bytes past the entries complete at open
  are reported as an orphaned tail). Prints a JSON report on stdout and a
  summary on stderr; exits 0 clean, 1 on any violation, 2 when unreadable.
  Each index crate gains a `verify` module checking every invariant its format
  carries (offset contiguity, CRC, framing, hash-chain linkage and tree-size
  deltas for compact blocks; size monotonicity, node canonicity, subtree end
  heights and rehashed nodes for tree state; row decoding, run order,
  duplicate/conflict detection and height order for the transparent runs), with
  a SHA-256 per file over the verified prefix. The cross-index pass checks
  tree-state hashes, times, sizes and leaves against the compact blocks, and
  that the transparent runs hold exactly the blocks' projection.
- `zainod` config gained a top-level `network` key (`mainnet` / `testnet` /
  `regtest`, default `mainnet`). Chain identity is declared, never derived:
  Zebra on regtest reports its chain as `"test"` over `getblockchaininfo`, so
  anything read off the validator mislabels regtest as testnet. It is threaded
  into `TreeStateService::new`, which exposes it as `TreeStateService::network`
  for `TreeState.network`.
- `index.<name>.enabled` is honoured. A disabled index opens no store, registers
  no queue on the fetch fan-out, boots no follower and claims no routes (its
  methods fall through to the validator fallback). `index.compact_block` cannot
  be disabled — the chain head trims against its finalised height and
  `GetLightdInfo` reports it — and `enabled = false` there is refused at
  startup.
- Ingest metrics, behind the `prometheus` feature: `zaino_best_tip` (the chain
  head's tip), `zaino_fetch_height` (highest contiguous height handed to the
  indexes; rewinds on a reorg), per index `zaino_index_finalized_height{index}`
  and `zaino_index_synced{index}` (0/1, the serving gate), and unlabelled
  per-pool work counters `zaino_fetch_{blocks,transactions,transparent_inputs,
  transparent_outputs,sapling_spends,sapling_outputs,orchard_actions,
  ironwood_actions}_total`. Emitted by `zainod` from watches it already holds
  and from the fetch `build` closure, so no stack crate depends on `metrics`.
  The names are a cross-repo contract with ztest's zainod backend, pinned by a
  scrape-level test. `zaino_finalized_tip` is gone (one source:
  `zaino_index_finalized_height{index="compact_block"}`).
- `IndexFollower::subscribe_synced`, beside `subscribe_finalized`.
- `zaino-chainview` — one view over N configured validators, replacing
  `zaino-mempool` (folded in, history preserved). Its poll/diff/publish loop is
  now the per-endpoint layer: each `EndpointPoller` owns its interval, backoff
  and failure count and reports deltas, so a dead validator degrades alone and
  the fold is `O(change)`. The aggregate publishes a `ChainViewSnapshot` through
  `ArcSwap` — a quorum tip (`⌊N/2⌋ + 1` over the **configured** set, the highest
  block that many endpoints agree on *by hash*), a mempool keyed by
  `Sighting { seen_at: EndpointSet, ours, raw }`, and per-endpoint
  `ValidatorMetadata`. Fails closed: below threshold the tip is `None` and
  `ChainViewSnapshot::mempool()` returns `BelowQuorum` rather than a weak
  answer. `ChainView::broadcast` fans a relay out to every endpoint, succeeds on
  any accept (a mixed result is a success), and marks the transaction `ours`, so
  a wallet sees its own send before it has propagated. `zainod` wires it at
  boot over `source` + the new `chainview_peers` list, booting one supervised
  poller per endpoint, and the frontend serves `SendTransaction`,
  `GetMempoolTx` and `GetMempoolStream` off it. `GetMempoolTx` prunes each
  `CompactTx` to the requested `pool_types`, where an empty list is the wire's
  legacy shielded set (Sapling, Orchard, Ironwood — transparent withheld); the
  consensus parse it needs arrives through a `ProjectCompact` port the daemon
  implements, so `zaino-grpc` stays free of any validator adapter.
  `ChainAction` carries
  `Sighted { txid, seen_at }` — propagation data the lightwalletd proto has no
  field for. `getpeerinfo` is read as partition/eclipse telemetry only, never as
  membership. **N=1 is a valid configuration** (threshold 1, quorum trivially
  met), so a single-validator deployment runs on this unchanged. Carries a
  `usage.md`.
- The gRPC frontend serves on hyper-util directly, with the tonic services
  mounted as routes, so the accept loop carries the serving caps: connection
  caps → metrics → admission → router. `max_connections` and
  `max_connections_per_ip` close a socket at accept; `max_streams` answers
  `UNAVAILABLE` with `grpc-retry-pushback-ms` rather than queueing;
  `max_streams_per_connection` is an h2 setting; `max_disk_reads` is the one
  bounded wait. A stream permit is owned by the response body, so it is returned
  when the stream ends *or* when the client disconnects. Configured by a new
  `[grpc]` section, and measured by the `zaino_rpc_*`, `zaino_connections_*` and
  `zaino_disk_read_wait_seconds` metrics (behind the `prometheus` feature).
- The tree-state and transparent-address indexes are served. `zaino-grpc`'s
  router resolves a method path to the index that claims it rather than letting
  the first wired index shadow the rest, and `zainod` opens, registers and boots
  all three sinks on the one fetch pipeline (`index.tree_state` and
  `index.transparent_address` config sections, each with its own storage
  directory). Newly answered from Zaino's own storage: `GetTreeState`,
  `GetLatestTreeState`, `GetSubtreeRoots`, `GetAddressUtxos`,
  `GetAddressUtxosStream`, `GetTaddressBalance`, `GetTaddressBalanceStream`.
  `GetTaddressTransactions` still falls through to the validator — it answers
  whole transactions, which needs a raw-transaction index.
- `zaino-index-tree-state` — the commitment-tree index. Stores the retained
  nodes of all three pools (every leaf, plus even indices at levels 1..31 —
  48 B/commitment), so the frontier at any historical tree size is reconstructed
  in ≤ 33 mmap reads with no hashing, rather than replayed from a checkpoint at
  ~150 ms per request. Serves `GetTreeState`, `GetLatestTreeState` and
  `GetSubtreeRoots`; subtree roots are a byproduct of the same fold, because an
  odd-indexed subtree root never appears as a frontier ommer. Pre-commit state is
  the non-finalised window: `apply` folds into `imbl` maps of retained nodes and
  per-height sizes, `finalize` lands a finalised run (folding whatever never
  entered pre-commit), and `reset` swaps the pre-commit carry for the durable one
  — so reads inside `tip − 1000` are answered rather than refused, and durable
  structures never delete. Both blocking hops move state by value through `spawn_blocking`,
  syncing only the level files that grew. Carries a `usage.md`.

- `zaino-index-transparent-address` — the t-address index, built on
  `zaino-runs` as two pure projections of one block (`receives` keyed by
  address, `spent` keyed by outpoint). The fold performs no lookups: no
  outpoint→address map, no UTXO set, nothing mutable. Serves `GetAddressUtxos`,
  `GetTaddressBalance` and `GetTaddressTransactions` by composing the two, and
  reports an unbuilt height as `FailedPrecondition` rather than an empty answer.
  Pre-commit state is the non-finalised window: the same fold, buffered in
  `imbl` maps above the runs and consulted before them, so queries inside
  `tip − 1000` are answered rather than refused, and a reorg is map truncation
  that never touches the append-only runs. Carries a `usage.md`.

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
- **Two more crates for the chain head subsystem** (ADR-0011), replacing
  `zaino-state`'s `non_finalised_state` module:
  - `zaino-chain-head` — the domain types and ports for the bounded,
    non-finalised head of the chain. No runtime and no data structures: the
    graph's representation belongs to whoever publishes it.
  - `zaino-chain-head-service` — the runtime: the writer task that keeps the
    graph reconciled with the validator, and the snapshots it publishes.

  The behavioural change this buys: the chain head and the finalised state now
  advance independently, so a slow database no longer holds the chain tip back.
  In exchange `ChainIndex::new` fails when the chain head cannot anchor, where
  the old code retried in the background indefinitely and served a "still
  syncing" case from every read path.
- Three mempool sourcing ports in `zaino-source` — `GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip` — all of which an adapter
  must route to the same transport as `GetMempoolTxids`.
- `[mempool]` config section in `zainod`, making the mempool memory bound, poll
  cadence and exclude-list caps operator-configurable.

### Changed
- **Crate consolidation: 25 workspace crates → 16.** Each merge keeps the
  absorbed crate's modules and moves its users over:
  - `zaino-persistence-codec` + `zaino-runs` → `zaino-persistence` (the storage
    core every index shares: record codec, `runs::{RunSet, RunWriter, ..}`,
    `verify`). The derive now emits `::zaino_persistence::` paths.
  - `zaino-logging` → `zainod` (`zainodlib::logging::init`).
  - `zaino-rpc` → `zaino-source-zebra-rpc` (the JSON-RPC transport is a private
    module; `RpcClient`, `RpcClientConfig`, `RpcError`, `ProbeError` and, behind
    `prometheus`, `metric_names` are re-exported).
  - `zaino-async`, `zaino-runtime` → deleted with `zaino-component` (see
    Removed).
  - `zaino-indexer` → `zaino-sync` (`BlockFetcher`, `SourceProvisioner`,
    `FanOut`); **breaking:** `IndexerError` is renamed `FetchError`, and
    `FanOut::push`/`reset` and `PushError` are crate-private.
  - `zaino-chain-head-service` → `zaino-chain-head` (see Removed).
- **`ChainHeadService::anchor` returns the writer alone**, and
  `ChainHeadService::subscribe_progress()` replaces
  `ChainHeadSubscriber::subscribe_progress()`. **Breaking.** The chain head no
  longer fetches commitment-tree roots (nothing read them), so
  `ChainHeadBlockSource` drops `GetCommitmentTreeRoots`.
- **`zaino_primitives::classify_script` no longer reads a 21-byte script as
  `[tag][hash20]`.** That arm existed only because the deleted state backend's
  on-disk data depended on it, and it classified any script opening `0x00` /
  `0x01` as standard — keying real funds under an address nobody controls.
- **`GetBlockRange` streams instead of buffering.** The response body is now an
  `http_body_util::StreamBody` fed from `RangeCursor`, with `grpc-status` in the
  trailers (a streaming body may not carry it in the headers); `GetBlock` and
  `GetLatestBlock` stay unary. The mmap copy behind it is bounded too: the span
  is copied one `SPAN_BUDGET` (1 MiB) window at a time rather than whole, so an
  unbounded range is no longer unbounded memory. An unprojected finalised window
  still leaves as one chunk.
- **`serve.max_block_range` caps one `GetBlockRange`** (default 131,072 = 2 ×
  the 2^16 subtree, since pepper-sync asks for a whole shard range in one call
  and never shrinks its ask). Over it is `invalid_argument` naming the limit and
  the ask — never a short answer, which a wallet reads as the end of the chain.
- **LMDB reader slots raised from 512 to 2048–8192.** The clamp was
  `(cpu * 32).clamp(512, 4096)`, which gives exactly 512 — the floor — on any
  host with 16 cores or fewer. With `NO_TLS` a slot belongs to a read
  *transaction* rather than a thread, so 512 is a hard ceiling on concurrent
  reads, and an ordinary concurrency benchmark exhausted it: reads failed with
  `MDB_READERS_FULL`, the startup block scan treats that as fatal, and the node
  restarted in a loop. A slot is one cache line (the measured `lock.mdb` is
  32,896 bytes at 512 readers), so 8192 slots costs ~512 KiB of shared memory.

  **This does not make exhaustion safe.** A client can still open more
  concurrent reads than there are slots. `MDB_READERS_FULL` being classified as
  a critical error — rather than the backpressure it is — remains an open bug,
  and until it is fixed a client can restart a node by exceeding whatever limit
  is configured.
- **Bulk finalised-state sync now assembles blocks concurrently too.** Making
  the fetch concurrent exposed the other half: a profile through the sandblast
  heights put **54% of all CPU on a single thread**, holding the run to 8
  blocks/s on 1.8 of 16 cores. That thread was the chainwork fold, which ran
  `assemble_indexed_block` in block order. Assembly is as expensive as the
  fetch and for the mirror-image reason — converting to the compact form
  re-serialises the Jubjub points zebra just decompressed, and returning to
  affine coordinates costs a field inversion per point. By Amdahl that capped
  the whole run at 1.85x however many cores the fetches used.

  A block's own proof-of-work depends only on its own header, so the cumulative
  chainwork is a running sum that can be folded over already-fetched blocks in a
  separate pass. Bulk sync is now three: fetch a window concurrently, fold the
  chainwork in order (integer arithmetic, microseconds), then assemble the
  window concurrently on `spawn_blocking`. The ordering guarantee is unchanged —
  every block still gets exactly its parent's chainwork plus its own.
- **Bulk finalised-state sync now fetches blocks concurrently.** A profile of a
  mainnet sync through the sandblast heights (~1.7M) put **91% of cycles in
  BLS12-381 scalar arithmetic** — `sqrt_tonelli_shanks`, `Scalar::square`,
  `Scalar::invert` — against **1.3% in LMDB**. That is Jubjub point
  decompression: zebra's block deserializer resolves `cv` and `ephemeral_key`
  for every Sapling output via `from_bytes_not_small_order` (a modular square
  root plus a cofactor multiplication), and Zaino discards all of it, keeping
  only the compact representation. Sandblast-era blocks carry hundreds of
  outputs each, which took block building from 1.4ms to 200ms per block — a
  sync doing 5 blocks/s on one core with fifteen idle.

  The fetch does not depend on `parent_chainwork`, so it no longer runs in block
  order: `write_blocks_to_height` now issues one fetch per core (less one, capped
  at 16) and folds the results back in height order. The read-state service runs
  each read on `spawn_blocking`, so the decompression spreads across cores. The
  order-dependent half — chaining `parent_chainwork` — is unchanged.

  This divides the waste rather than removing it. The fix that removes it is
  upstream in zebra: decompress `cv`/`ephemeral_key` lazily, since reading a
  historical block never verifies it.

  `zaino.sync.block_build_seconds` still records per-block cost, so with N
  fetches in flight its sum approaches N x wall-clock; divide by the concurrency
  before comparing it against elapsed time.
- **Bulk finalised-state sync now pipelines its write batches.**
  `DbV1::write_blocks_to_height` commits batch N on a scoped thread while it
  builds batch N+1, instead of alternating between the two. Measured on a
  mainnet sync at height ~1.2M, the serial form spent 60% of wall-clock building
  blocks and 40% inside `write_block_batch_blocking` + `env.sync`, with the
  builder idle for every commit — a mean 79s pause every 120s. Overlapping them
  hides the shorter half behind the longer.

  Durability and resume semantics are unchanged: a batch is still written in one
  transaction and fsynced before the validated tip advances, so the on-disk
  `headers` tip never runs ahead of the indexes. The one operational change is
  memory — peak heap for buffered blocks is now up to *twice*
  `storage.database.sync_write_batch_size`, since the batch being committed is
  still resident while its successor fills. That knob bounds one batch, not the
  pipeline; size it for the host accordingly.
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
- **`GetBlockRangeNullifiers` and `GetTaddressTxids` are served again, as
  deprecated aliases — TODO: REMOVE.** pepper-sync calls both on every sync and
  fails the sync on any non-OK status, so without them no pepper-sync wallet can
  sync against Zaino. `GetTaddressTxids` is `GetTaddressTransactions` under its
  old name (same request, same response); `GetBlockRangeNullifiers` is
  `GetBlockRange` re-projected to nullifiers only, with `TRANSPARENT` in
  `poolTypes` ignored as the proto requires. Both carry `option deprecated` in
  the proto and a removal marker at every site; delete them once pepper-sync
  calls the replacements.
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
- **Zaino serves no JSON-RPC. Breaking.** `zaino-noderpc` is deleted, and with
  it `serve.jsonrpc_listen_address`: its six methods (`getbestblockhash`,
  `getblockchaininfo`, `getblockcount`, `getmininginfo`, `gettxout`,
  `sendrawtransaction`) only forwarded to the validator. `ServeConfig` denies
  unknown fields, so a config still carrying the key fails to parse. Ask the
  validator's own JSON-RPC instead.
- **`zaino-status` is deleted. Breaking.** `StatusType`, `NamedAtomicStatus`,
  `Status`, `Liveness`, `Readiness` and `VitalsProbe` were a second status
  vocabulary; the chain head, its only user, logs its transitions through
  `tracing` instead.
  `ChainHeadService::status`/`shutdown` and `ChainHeadSubscriber` go with it.
- **The chain head's query surface is deleted. Breaking.**
  `ChainHeadSnapshot`, `ChainHeadTransactionService`,
  `ChainHeadTransactionLocations`, `ChainHeadTxPosition`, `SpenderLocation`,
  `ChainHeadBlockIter`, `ChainHeadError`, `ChainHeadBlockService`, the epoch /
  generation stamping and the `testing` feature had no consumer outside the
  crate. The hash-keyed block graph (reorg detection, most-work best chain) and
  `ChainProgress { tip, reorgs }` stay. `zaino_primitives::ChainStateEpoch`,
  `Outpoint` and `BlockTxPosition` are deleted with them.
- **`zaino-component` is deleted (with `zaino-runtime` and `zaino-async`).
  Breaking.** `Orchestra`, `RunLoop`, `RunReporter`, `RunComponent`,
  `ServeComponent`, `ValidatorComponent`, `ReachabilityProbe`, `Task`, the
  status/health/lifecycle vocabulary and `HealthServer` are gone. `zainod`
  spawns each stage as a plain task (`ChainHeadService::run`,
  `EndpointPoller::run`, `BoundGrpcServer::run`, each taking a
  `tokio_util::sync::CancellationToken`); any task ending before a shutdown
  signal restarts the daemon. `GrpcServer::bind()` takes the socket at boot,
  so a bind failure is still a boot failure. Retry and readiness transitions
  are `tracing` lines.
- **`GetBlockNullifiers` and `Ping` are gone from the gRPC surface.**
  `GetBlockNullifiers` was deprecated in the proto in favour of `GetBlock`;
  `Ping` was testing-only. The `Duration` and `PingResponse` messages went with
  `Ping`; nothing else referenced them.
- **`zaino-consensus` is deleted from the workspace.** Zaino is an indexer and
  carries no consensus logic (`docs/design/boundaries.md`): its raw-transaction
  size/hex validation (`validate_raw_transaction_bytes`,
  `validate_raw_transaction_hex`, `RawTransactionError`, `MAX_BLOCK_BYTES`) had
  no consumers and is gone — Zaino relays, Zebra validates and rejects. The
  protocol constants moved to `zaino_primitives::protocol`, unchanged; the
  drift-guard test against `zebra-chain` stays in `zaino-source-zebra-rpc`,
  which already depends on it.
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
- **An index no longer panics when an initial sync reaches the tip.** Blocks
  below the finality boundary are staged for a batched write, and the tip's
  blocks are applied to pre-commit. The follower handed `apply` the first
  pre-commit block while a partial batch was still staged beneath it, so the
  sink's contiguity assertion fired unless the bulk span was an exact multiple
  of `batch`. The follower now writes the staged blocks first.
- **A caught-up index now serves when it reaches the tip, not ~`batch` blocks
  later.** The serving gate was recomputed only when a batch was written, which
  at the tip happens once every `batch` blocks (~a day on mainnet at the default
  1000). It is recomputed whenever the follower's queue drains, so a burst
  already queued cannot close it between its own blocks, and a reorg reset
  closes it immediately.
- `GetLightdInfo` now fills `chainName` (from config, as lightwalletd names
  chains), `saplingActivationHeight`, `consensusBranchId`, and
  `upgradeName`/`upgradeHeight` (the next *pending* upgrade) from the
  validator's `getblockchaininfo`, whose `estimatedheight` is now what
  `estimatedHeight` reports. With the validator unreachable it still answers,
  with those fields empty rather than guessed.
- An index that is still building now answers `FailedPrecondition`, not
  `Unimplemented`. `Unimplemented` tells a client the method will never work, so
  it retires it permanently rather than retrying once the index catches up.
- `z_gettreestate` wrote the Orchard and Ironwood `finalRoot` byte-reversed. The
  reversal that turns a Sapling root into display order is Sapling's alone — a
  Pallas root's `to_repr` is already display order — so both pools named a root
  no chain ever had.
- `getrawtransaction` in verbose mode omitted `time` and `blocktime`. Both come
  from the containing block's header, which the index already holds.
- The read-state backend's `getblockchaininfo` reported only `sprout`, `sapling`
  and `orchard`, leaving `transparent`, `lockbox` and `ironwood` reading as
  empty pools — so the Ironwood pool appeared to hold nothing across NU6.3
  activation. It also reported `chainSupply` as the transparent balance rather
  than the total over every pool.
- `chainSupply` was rendered as zero on every backend: the wire conversion
  recognised the unnamed total but discarded its value.
- `GetBlock` and `GetBlockNullifiers` served every pool unconditionally, while
  `GetBlockRange`/`GetBlockRangeNullifiers` honoured the request's `poolTypes`
  and default to the legacy shielded-only set. The same height therefore came
  back with different contents depending on which RPC asked for it — a
  transparent-only transaction was present in the single-block form and absent
  from the range form. `BlockID` carries no `poolTypes` field, so both
  single-block RPCs now serve the unfiltered default, matching both the range
  form and lightwalletd.
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
- added `getaddressdeltas` stub to json-rpc server
- Errors relayed from backing validator properly propagate the error message
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
