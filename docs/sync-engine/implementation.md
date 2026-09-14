# Sync Engine Implementation

This document describes the concrete implementation of the sync engine as it
exists in the `review-sync` branch. It is written for a developer who has read
the [design document](../sync-engine-design.md) and the
[formal model](../index-sync-model.md) but has not yet explored the code.

---

## Context

Zaino's original `zaino-state` crate grew into a monolith. The `BlockchainSource`
trait in `zaino-state/src/chain_index/source.rs` accepted a `HashOrHeight`, returned
an `Arc<zebra_chain::block::Block>`, and mixed transport concerns (JSON-RPC envelope
parsing, Zebra `ReadStateService` tower calls, mempool queries, treestate retrieval,
subtree-root pagination) into a single god-trait with over a dozen methods. Every
consumer imported `BlockchainSource` and transitively pulled in Zebra types, the
`zaino-fetch` HTTP client, protocol-buffer definitions, and the LMDB storage layer.
Adding a new data source or swapping the persistence engine required touching code
throughout the tree.

The sync engine redesign replaces this with a hexagonal architecture. Domain logic
lives at the centre, behind trait boundaries that separate the three external
concerns -- where blocks come from (source adapters), how extracted data is stored
(persistence adapters), and what computation turns raw blocks into indexed entries
(index definitions). The formal model provides the theoretical framework: every index
declares its input scope and composition type, and the engine derives the schedule,
parallelism strategy, and merge semantics from those declarations alone.

---

## Crate Architecture

The implementation is split across ten crates organised into four dependency layers
that mirror the design document's conceptual layers, plus conversion and transport
crates that sit outside the core dependency chain.

At the foundation sits `zaino-primitives`, a zero-dependency crate that defines the
vocabulary types every other crate needs: `Height`, `BlockHash`, `TransactionHash`,
`Block`, `BlockHeader`, `Transaction`, and the shielded and transparent sub-structures.
Nothing in the system depends on Zebra types or storage types for its core domain
model; everything passes through `zaino-primitives` first.

The two driven-port crates define the trait boundaries. `zaino-source` declares
one trait per question a consumer can ask about the chain -- `GetBlock`, `GetChainTip`,
`GetTransaction`, `GetAddressBalance`, and so on -- each with its own per-trait error
type. Consumers compose capabilities via trait bounds (`fn sync<V: GetBlock + GetChainTip>`),
and adapters implement whichever subset they support. `zaino-persistence` declares the
storage interface: `Backend`, `BackendReader`, `BackendWriter`, and `WriteOp`. Both
crates include mock implementations behind a `testing` feature flag.

The sync engine itself lives in `zaino-sync`. This is the generic, blockchain-agnostic
orchestrator. It contains the trait hierarchy (`IndexDef`, `ExtractLocal`,
`ExtractCumulative`, `MergeAppend`, `MergeMonoidal`, `MergeFold`, `Schema`), the
dependency DAG builder, the scheduler with phantom-typed batch handles, the
`BlockBuffer` sliding window, the bridge layer that erases index types for dynamic
dispatch, and the `SyncEngine` orchestrator with its three entry points. Nothing in
this crate knows about Zcash, LMDB, or JSON-RPC.

Blockchain-specific index definitions live in `zaino-indexes`. Each index module
declares a narrow context type, implements `IndexDef` with compile-time scope and
composition markers, provides an extraction function, and defines a `Schema` for
persistence encoding. Index set modules compose indexes into named configurations,
define the set-wide context type, and provide `ProvideContext` projections. Currently
two sets exist: `headers_only` (a single `HeadersIndex`) and `headers_and_spends`
(`HeadersIndex` plus `TransparentSpendsIndex`).

Adapter crates implement the driven-port traits against concrete technologies. On the
source side, `zaino-source-zebra-rpc` bridges `zaino-source` query traits to a Zebra
validator over JSON-RPC using the shared `zaino-rpc` HTTP client, while
`zaino-source-zebra-readstate` opens Zebra's finalized-state RocksDB directly for
zero-copy reads. On the persistence side, `zaino-backend-lmdb` implements `Backend`
against LMDB with one named database per namespace. The conversion crate
`zaino-convert-zebra` maps `zebra_chain` types into `zaino-primitives` domain types,
providing both full-block conversion (`block_from_zebra`) and a fast header-only path
(`header_from_parts`) that avoids deserialising transactions entirely.

The dependency graph flows strictly inward: adapter crates depend on port-trait crates
and `zaino-primitives`; `zaino-indexes` depends on `zaino-sync` and `zaino-primitives`;
`zaino-sync` depends on `zaino-persistence` (for the `Backend` trait it orchestrates
against). No adapter depends on another adapter. No domain crate depends on an adapter.

---

## How New Crates Refactor Existing Code

The old `BlockchainSource` trait in `zaino-state` combined block fetching, transaction
lookup, mempool access, treestate queries, subtree-root pagination, address balance
lookups, and chain-tip polling into a single interface. Both the `FetchService`
(JSON-RPC client) and `StateService` (Zebra `ReadStateService` wrapper) had to
implement every method, and callers that only needed block data still pulled in the
full dependency tree.

`zaino-source` decomposes this into fine-grained traits. `GetBlock` returns a domain
`Block` for a height. `GetChainTip` returns the tip height and hash. `GetTransaction`
fetches by txid. Each trait has a dedicated error type, and each adapter implements
only what it supports. The resilience wrapper (`Resilient<A>`) decorates any adapter
with retry and backoff logic without baking retry into each implementation.

The old `zaino-fetch` crate contained both the JSON-RPC HTTP client and the
response-parsing logic for each RPC method. The new `zaino-rpc` crate retains only
the transport layer -- HTTP requests, JSON-RPC envelope, authentication, and
work-queue-exhaustion retry -- and returns raw `serde_json::Value`. Response parsing
moves into the adapter crate (`zaino-source-zebra-rpc`), where it belongs.

The old LMDB code in `zaino-state` was entangled with the indexing logic.
`zaino-persistence` extracts the storage contract into `Backend`, `BackendReader`, and
`BackendWriter` traits that know nothing about block structure or index semantics.
`zaino-backend-lmdb` implements these traits with LMDB-specific configuration
(`NO_SYNC` for batch-boundary flushing, `NO_TLS` for cross-thread read transactions)
while the in-memory backend in `zaino-persistence` itself serves testing.

The old block-parsing code spread across `zaino-state/src/chain_index/` used Zebra
types directly throughout the indexing pipeline. `zaino-convert-zebra` centralises
all Zebra-to-domain conversions in one place, producing `zaino-primitives` types that
the rest of the system consumes. The header-only conversion path
(`header_from_parts`) accepts pre-parsed header components from Zebra's
`ReadRequest::BlockHeader` response, avoiding full block deserialisation entirely --
a critical optimisation for the 174k blocks/s throughput observed in header-only sync.

---

## The Sync Pipeline

A block's journey from source to persisted index entry passes through five stages,
each owned by a different component.

The provisioner (currently `MockProvisioner` for tests, with real adapters pending)
fetches blocks from the source and produces a set-wide context value -- for instance
a `HeadersAndSpendsContext` containing the header fields and the list of transparent
spends extracted from the block's transactions. This context is the union of everything
any index in the set might need.

The engine feeds contexts into the `BlockBuffer`, a sliding-window `BTreeMap<u32, Arc<Ctx>>`
that holds blocks for the current and potentially next batch. Backpressure is implicit:
when the buffer reaches its capacity, the supply phase stalls until batch eviction
frees space.

The `Scheduler` tracks per-index extraction progress and emits `Task` values. For each
block in the current batch, it produces `Task::Extract(ExtractJob)` entries specifying
which index should process which block offset. The engine dispatches these extractions
in parallel via Rayon's `par_iter`. Each extraction call goes through the
`IndexPipeline` trait-object interface: the engine calls `pipeline.extract_one(&ctx)`,
and the bridge inside projects the set-wide context down to the index's narrow context
type via `ProvideContext`, then calls the index's statically-typed `ExtractLocal::extract`
(or `ExtractCumulative::extract` for S-scope indexes). The delta is stored inside the
bridge's interior-mutable buffer.

When all extractions for a batch complete, the scheduler emits
`Task::CompleteBatch(BatchHandle<FullyExtracted>)`. The engine calls `pipeline.merge()`
on each index, which invokes the appropriate `MergeStrategy`: for Append indexes, the
deltas are simply collected; for Monoidal indexes, they are reduced via the declared
monoid; for Fold indexes, they are applied sequentially. The bridge then calls
`pipeline.persist()`, which invokes the index's `Schema` implementation to convert the
merged result into typed key-value entries, then encodes those entries into raw bytes
via `Schema::encode_key` and `Schema::encode_value`, producing a `Vec<WriteOp>`.

The engine collects `WriteOp` vectors from all indexes in the batch, appends a
watermark entry recording the committed height, and passes the combined batch to
`BackendWriter::commit()` in a single atomic transaction. After commit, it calls
`Backend::flush()` to force durability. The scheduler advances its watermark, the
`BlockBuffer` evicts consumed entries, and the loop continues with the next batch.

For `SelfCumulative` indexes, the `CumulativeBridge` threads accumulated state across
blocks within a batch (via its internal `running_state` field) and across batches
(via `load_state` from the backend on resume). This ensures that an S-scope index
interrupted mid-sync can pick up exactly where it left off.

---

## Index Definition Pattern

Defining a new index requires four trait implementations and no concurrency,
scheduling, or persistence code. The `HeadersIndex` in `zaino-indexes` illustrates
the pattern.

First, the index module declares a narrow context type containing only the data this
index needs. For `HeadersIndex`, that is `HeaderCtx` with five fields: height, hash,
prev_hash, time, and bits. This context is distinct from the set-wide context; the
set module provides the projection via `ProvideContext<HeaderCtx>`.

Second, the `IndexDef` implementation pins the index on the scope-composition grid.
`HeadersIndex` sets `type Scope = BlockLocal` and `type Composition = Append`, meaning
its extraction needs only the current block and its entries have disjoint keys. The
`Delta` associated type is `HeaderEntry` -- a struct carrying the height as key and a
`HeaderValue` (hash, prev_hash, time, bits) as value. The `const NAME: IndexId`
identifies the index for scheduling, persistence namespace mapping, and diagnostics.

Third, the scope-specific extraction trait is implemented. Because `HeadersIndex` is
`BlockLocal`, it implements `ExtractLocal`, whose signature is
`fn extract(ctx: &HeaderCtx) -> Result<HeaderEntry, ExtractError>`. The implementation
simply copies the context fields into a `HeaderEntry`. A `SelfCumulative` index would
instead implement `ExtractCumulative`, receiving its prior accumulated state as a
second parameter. A `CrossIndex` would implement `ExtractCross`, receiving a
`DepsReader` handle to query committed dependency state. The compiler prevents an
L-scope index from accessing prior state or dependency data -- the parameters are
simply not in the signature.

Fourth, the `Schema` implementation defines the persistence encoding. For Append
indexes, the generic parameter is `Vec<Delta>` (the collected deltas from the batch).
`Schema::into_entries` maps the batch into typed `(Key, Value)` pairs, and
`encode_key`/`encode_value` serialise them to bytes. The reverse path --
`decode_key`, `decode_value`, and `from_entries` -- supports state loading for
cumulative indexes and future query serving. For `HeadersIndex`, the key is a
`BlockHeight` encoded as 8 little-endian bytes, and the value is a 72-byte fixed-width
encoding of the four header fields.

The merge trait implementation is trivial for Append indexes: `impl MergeAppend for HeadersIndex {}`
is a marker with no methods. Monoidal indexes would implement `MergeMonoidal` with
`identity`, `lift`, and `combine`; Fold indexes would implement `MergeFold` with
`initial_state` and `fold`.

Composing indexes into a set is a separate concern, handled in the `sets` module.
A set module defines the set-wide context type (the union of all per-index contexts),
implements `ProvideContext` for each index's narrow type, and exposes a builder
function: `IndexSet::new().with::<HeadersIndex>().with::<TransparentSpendsIndex>()`.
The same index definition can appear in multiple sets with different set-wide contexts;
only the `ProvideContext` projection differs.

---

## Error Model

Errors are stratified into three layers, each with a distinct audience and retry
semantic.

At the boundary with external systems, `zaino-source` defines a three-tier error
hierarchy. `FetchError` represents a single transport-level failure (connection
refused, timeout, HTTP status, RPC error code, parse failure, auth rejection), carrying
a machine-readable `FailureMode` enum that the resilience wrapper matches on to decide
retryability. `QueryError<E>` wraps either a domain rejection from the server (the
`Domain` variant, parameterised by the per-trait error type) or a `FetchError` --
this is what adapter implementations return. `SourceError<E>` is the consumer-facing
type from the resilience wrapper, adding an `Unavailable` variant for retries
exhausted. Domain errors are never retried; transport errors may be; unavailability
is terminal for the current operation.

`zaino-persistence` takes a simpler, per-operation approach. `OpenError` covers
handle acquisition (backend closed, corrupted). `CommitError` covers write failures
(namespace not found, IO error, transaction conflict). `ReadError` covers read
failures. `FlushError` covers durability failures. Each is a small `thiserror` enum
with string payloads -- the backend adapter fills in implementation-specific detail.

The sync engine composes both layers into `SyncError`, which wraps `DagError` (invalid
dependency graph), `PipelineError` (extraction or merge failure within an index
pipeline), and the four persistence error types via `#[from]` conversions. `PipelineError`
itself wraps `ExtractError` (from index extraction code) and adds `Merge` and `Persist`
variants for failures during those pipeline phases. This layering means that a
transport timeout in the provisioner, a parse error in an index's extraction function,
a namespace-not-found in the LMDB backend, and a cycle in the dependency DAG all
surface as distinct, pattern-matchable variants of `SyncError` without losing their
original context.

---

## Benchmark Results

Three benchmark configurations have been run against Zcash mainnet data, each
exercising a different combination of source adapter and index set.

The headers-only configuration using `ZebraReadStateAdapter` achieved 174,000
blocks per second. This configuration uses `header_from_parts` in
`zaino-convert-zebra`, which accepts Zebra's pre-parsed `Header` struct and height
without deserialising the block body at all. The index set contains only
`HeadersIndex` (BlockLocal, Append), so extraction is embarrassingly parallel with
no merge overhead. This number represents the ceiling for the sync engine's
scheduling and persistence machinery when extraction cost is near zero.

The headers-plus-transparent-spends configuration using `ZebraReadStateAdapter`
achieved 539 blocks per second. Adding `TransparentSpendsIndex` forces full block
deserialisation through `block_from_zebra`, which parses every transaction's
transparent inputs, sapling nullifiers, and orchard actions. The three-orders-of-magnitude
drop from the headers-only case is almost entirely attributable to block
deserialisation cost (particularly the `sandblast` region of the chain where
individual blocks contain thousands of shielded outputs) rather than sync engine
overhead.

The RPC-based configuration using `ZebraRpcAdapter` with the headers-and-spends
index set achieved 323 blocks per second on the sandblast region. The additional
overhead versus the ReadState path comes from HTTP round-trips, hex encoding of
block bytes in JSON-RPC responses, and JSON parsing. For the pre-sandblast region
where blocks are small, RPC throughput is substantially higher, but the sandblast
blocks (which dominate total sync time) are the meaningful benchmark.

---

## What's Not Yet Implemented

Several components described in the design document remain as stubs or placeholders.

CrossIndex bridges are structurally present but non-functional. The `DepsReader`
type exists but has no methods; `ExtractCross` compiles but cannot provide dependency
data to an extractor. The scheduler's `Barrier` firing rule is declared but always
blocks. No real Zcash index currently requires X-scope, so this is deferred until
one does.

The `SourceHandle` and `NonLocalSource` escape hatches are placeholder structs. These
would allow an index to declare `SourceAccess::NonLocal` and receive a handle for
fetching data about blocks other than the one being extracted -- useful for cases where
adding an intermediate dependency index is disproportionate. The engine would adjust
scheduling for I/O latency when this is declared.

The streaming provisioner interface remains synchronous. The `Provisioner` trait's
`provision_range` method returns `Vec<BlockContext>`, suitable for tests and small
ranges. The `sync_channel` entry point on the engine accepts a `tokio::mpsc::Receiver`
and is the intended production path (provisioner on a separate task, feeding blocks
through a channel with backpressure), but no real provisioner implementation wires
this up yet.

The real Zcash index set is incomplete. Of the roughly ten indexes needed for full
Lightwalletd-compatible serving (headers, block heights, txid location, transparent
inputs/outputs, sapling spends/outputs, orchard actions, commitment trees, spent
markers, address history), only `HeadersIndex` and `TransparentSpendsIndex` have
implementations. The remaining indexes follow the same pattern and are
straightforward to add.

Reorg handling is not addressed. The engine covers initial sync (genesis to tip);
steady-state tip-following with rollbacks has different characteristics (batch size
of 1, undo logic that depends on composition type). The formal model notes this as
future work.

The `zainod` binary has not been wired to use the new sync engine. The integration
point would be an `IndexSet` registration at startup, a provisioner backed by the
configured source adapter, and an `LmdbBackend` backed by the configured database
path, all composed into a `SyncEngine` that replaces the current monolithic sync loop.
