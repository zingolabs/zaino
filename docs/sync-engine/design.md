# Sync Engine Implementation Design

Concrete architecture for a DAG-driven parallel sync engine, based on the formal
model in [index-sync-model.md](index-sync-model.md).

This document covers the implementation design — crate structure, trait hierarchy,
data flow, and runtime behaviour. The formal model covers the theory — axes,
invariants, cost model.

User stories derived from this design live in
[sync-engine-user-stories.md](sync-engine-user-stories.md).

---

## 1. Architecture Overview

Four layers, each independently testable and swappable:

```
┌─────────────────────────────────────────────┐
│ Layer 1: Index Definitions                  │
│   Descriptor + extract + merge + write_ops  │
│   (blockchain-specific)                     │
└──────────────────┬──────────────────────────┘
                   │ registered into
┌──────────────────▼──────────────────────────┐
│ Layer 2: Sync Engine                        │
│   DAG, scheduling, parallel dispatch,       │
│   merge, commit, flush                      │
│   (generic — no blockchain knowledge)       │
└───┬──────────────────────────────┬──────────┘
    │                              │
┌───▼──────────────┐  ┌───────────▼───────────┐
│ Layer 3:         │  │ Layer 4:              │
│ Provisioner      │  │ Backend               │
│                  │  │                       │
│ source → context │  │ commit + flush + read │
│ (blockchain-     │  │ (storage-specific)    │
│  specific)       │  │                       │
└──────────────────┘  └───────────────────────┘
```

---

## 2. Core Invariant

Every extraction receives inputs determined by its **scope**:

| Scope | Extract signature | Inputs |
|-------|-------------------|--------|
| $\mathsf{L}$ (BlockLocal) | `extract(ctx) -> Delta` | BlockContext only |
| $\mathsf{S}$ (SelfCumulative) | `extract(ctx, prior) -> Delta` | BlockContext + own prior accumulated state |
| $\mathsf{X}$ (CrossIndex) | `extract(ctx, deps) -> Delta` | BlockContext + DepsReader over committed deps |

The scope axis is enforced at the type level: each scope has its own extract
trait (`ExtractLocal`, `ExtractCumulative`, `ExtractCross`), so an L-scope
index literally cannot ask for prior state or dependency reads — the
signature doesn't have the parameter.

**Invariants preserved:**
- No extractor calls the source directly.
- No extractor reads uncommitted state.
- No extractor accesses data about blocks other than $h$ through the source.
- Non-local data about earlier blocks comes from committed indexes via DepsReader
  (X-scope) or from engine-threaded accumulated state (S-scope).

An explicit escape hatch (`SourceAccess::NonLocal`) exists for cases where adding
an intermediate index is disproportionate to the need. When declared, the engine
provides a source handle and adjusts scheduling for I/O latency. *(placeholder
only — `SourceHandle` / `NonLocalSource` not yet implemented)*

> **Note:** The original invariant described a uniform two-input signature
> `f(BlockContext, DepsReader)` for all scopes. The implementation replaces
> this with scope-specific trait signatures that make invalid access
> unrepresentable at compile time. The S-scope's `PriorState` is a third
> input channel the original formulation did not name — it is threaded by
> the engine (loaded from backend on resume, carried across blocks/batches
> by the bridge), not provided by the provisioner or the DepsReader.

---

## 3. Crate Structure

```
zaino-sync/                      ← generic, reusable, no blockchain knowledge
  src/
    lib.rs                       ← public API surface
    descriptor.rs                ← InputScope, CompositionType, SourceAccess, Descriptor
    traits.rs                    ← IndexDef, ExtractLocal/Cumulative/Cross,
                                    MergeAppend/Monoidal/Fold, Schema<M>,
                                    ProvideContext, WriteOp
    encode.rs                    ← Encode / Decode traits + built-in impls
    dag.rs                       ← DependencyDag, FiringRule, acyclicity, phase assignment
    scheduler.rs                 ← Scheduler, Task, ExtractJob, BatchHandle<State>
    engine.rs                    ← SyncEngine: supply/demand loop, entry points
    block_buffer.rs              ← BlockBuffer: sliding window, batch eviction
    pipeline.rs                  ← IndexPipeline (trait-object-safe), IntoIndexPipeline
    bridge.rs                    ← LocalBridge, CumulativeBridge, MergeStrategy,
                                    BridgeDispatch (sealed)
    backend.rs                   ← Backend, BackendReader, BackendWriter, WriterTopology
    provisioner.rs               ← Provisioner trait (generic)
    progress.rs                  ← SyncProgress, watermark tracking, crash recovery
    primitives.rs                ← BlockHeight, IndexId, BatchIndex, PhaseIndex, BlockOffset
    testing.rs                   ← InMemoryBackend, SlowBackend, TestBlockContext,
                                    MockProvisioner
    testing/
      toy_indexes.rs             ← ValueIndex (L,A), CountIndex (L,M),
                                    RunningSumIndex (L,F), CumulativeSumIndex (S,M)

zaino-state/                     ← existing crate
  src/
    chain_index/                 ← existing read-side code, UNCHANGED

    indexes/                     ← NEW: per-index implementations (not yet started)
      headers.rs                 ← (L,A) impl ExtractLocal + MergeAppend
      block_heights.rs           ← (L,A)
      txid_location.rs           ← (L,A)
      transparent.rs             ← (L,A)
      sapling.rs                 ← (L,A)
      orchard.rs                 ← (L,A)
      commitment_tree.rs         ← (L,A) or possibly (S,M)
      spent.rs                   ← (L,A)
      addr_history.rs            ← (L,M) impl ExtractLocal + MergeMonoidal
      txoutset_accum.rs          ← (L,M) impl ExtractLocal + MergeMonoidal

    sync/                        ← NEW: wiring (not yet started)
      provisioner.rs             ← impl Provisioner for Zcash (source → ZcashBlockContext)
      backend_lmdb.rs            ← impl Backend for LMDB
      strategy.rs                ← SyncStrategy trait + MonolithicSync + DagSync impls
```

---

## 4. Layer 1: Index Definitions

### 4.1 Descriptor

Static, declarative properties. No logic. The type-level markers (`BlockLocal`,
`SelfCumulative`, `CrossIndex`, `Append`, `Monoidal`, `Fold`) are sealed traits
with runtime mirrors for dynamic dispatch.

```rust
struct Descriptor {
    name: IndexId,
    scope: InputScope,
    composition: CompositionType,
    dependencies: &'static [IndexId],
    source_access: SourceAccess,
}

// Type-level markers (sealed)
struct BlockLocal;       // implements Scope
struct SelfCumulative;   // implements Scope
struct CrossIndex;       // implements Scope
struct Append;           // implements Composition
struct Monoidal;         // implements Composition
struct Fold;             // implements Composition

// Runtime mirrors
enum InputScope { BlockLocal, SelfCumulative, CrossIndex }
enum CompositionType { Append, Monoidal, Fold }
enum SourceAccess { None, NonLocal }
```

### 4.2 Context Projection (replaces SourceRequirements)

~~The original design used `SourceRequirements` bitflags to configure the
provisioner at runtime.~~ The implementation uses **compile-time context
projection** via the `ProvideContext` trait instead:

```rust
/// Project set-wide Ctx → per-index BlockContext.
trait ProvideContext<T> {
    fn context(&self) -> T;
}

// Identity blanket: if the index wants the whole context, it's free.
impl<T: Clone> ProvideContext<T> for T {
    fn context(&self) -> T { self.clone() }
}
```

The set-wide `Ctx` type parameter on `SyncEngine<Ctx, B>` is the provisioner's
output. Each index declares its own `IndexDef::BlockContext`, and the engine
requires `Ctx: ProvideContext<I::BlockContext>` for every registered index.

This means the provisioner always fetches the full context (the union is
implicit in the `Ctx` type), which is simpler but loses the runtime "fetch
only what's needed" optimisation. If that becomes a bottleneck, context
projection can be extended with a `SourceRequirements` bitflag approach at
the provisioner level without changing index code.

### 4.3 Index Definition and Extraction Traits

The root trait `IndexDef` declares scope and composition at the type level.
Extraction is split into scope-specific traits; merge is split into
composition-specific traits.

```rust
/// Root: declares position on the Scope × Composition grid.
trait IndexDef: Send + Sync + 'static {
    type Scope: Scope;          // type-level: BlockLocal | SelfCumulative | CrossIndex
    type Composition: Composition;  // type-level: Append | Monoidal | Fold
    type Delta: Send + Sync;
    type BlockContext: Send + Sync + 'static;

    const NAME: IndexId;
    const DEPENDENCIES: &'static [IndexId] = &[];
    const SOURCE_ACCESS: SourceAccess = SourceAccess::None;

    fn descriptor() -> Descriptor { /* derived from above */ }
}

/// L-scope: pure block-level extraction. Full parallelism.
trait ExtractLocal: IndexDef<Scope = BlockLocal> {
    fn extract(ctx: &Self::BlockContext) -> Result<Self::Delta, ExtractError>;
}

/// S-scope: extraction depends on own prior accumulated state.
trait ExtractCumulative: IndexDef<Scope = SelfCumulative> {
    type PriorState: Send + Sync;
    fn extract(ctx: &Self::BlockContext, prior: &Self::PriorState)
        -> Result<Self::Delta, ExtractError>;
}

/// X-scope: extraction depends on committed state of declared dependencies.
trait ExtractCross: IndexDef<Scope = CrossIndex> {
    fn extract(ctx: &Self::BlockContext, deps: &DepsReader)
        -> Result<Self::Delta, ExtractError>;
}
```

### 4.4 Merge Traits

```rust
/// Append: disjoint keys. Marker trait — no merge logic needed.
trait MergeAppend: IndexDef<Composition = Append> {}

/// Monoidal: associative + commutative combine.
trait MergeMonoidal: IndexDef<Composition = Monoidal> {
    type Accumulator: Send + Sync;
    fn identity() -> Self::Accumulator;
    fn lift(delta: Self::Delta) -> Self::Accumulator;
    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator;
}

/// Fold: order-dependent sequential application.
trait MergeFold: IndexDef<Composition = Fold> {
    type FoldState: Send + Sync;
    fn initial_state() -> Self::FoldState;
    fn fold(state: &mut Self::FoldState, delta: Self::Delta);
}
```

### 4.5 Schema and Persistence Traits (replaces to_write_ops)

~~Each merge trait originally had a `to_write_ops` method coupling merge
output directly to WriteOps.~~ The implementation separates this into two
layers:

```rust
/// Typed persistence: merge result → typed entries.
/// Generic over M (the merge result type):
///   Vec<Delta> for Append, Accumulator for Monoidal, FoldState for Fold.
trait Schema<M>: IndexDef {
    type Key: Encode + Decode + Send + Sync;
    type Value: Encode + Decode + Send + Sync;
    fn into_entries(merged: M) -> Vec<(Self::Key, Self::Value)>;
    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> M;
}

/// Byte-level serialisation boundary.
trait Encode { fn encode(&self) -> Vec<u8>; }
trait Decode: Sized { fn decode(bytes: &[u8]) -> Result<Self, DecodeError>; }
```

This separation enables versioned schema types (different `Key`/`Value` per
DB version) without changing extraction or merge logic.

### 4.6 Bridges and MergeStrategy

The engine operates on trait-object-safe `IndexPipeline<Ctx>` values. Bridges
adapt the statically-typed index traits to this dynamic interface:

```rust
/// Trait-object-safe interface with interior mutability (Mutex).
trait IndexPipeline<Ctx>: Send + Sync {
    fn descriptor(&self) -> &Descriptor;
    fn extract_one(&self, ctx: &Ctx) -> Result<(), PipelineError>;
    fn merge(&self) -> Result<(), PipelineError>;
    fn persist(&self) -> Result<Vec<WriteOp>, PipelineError>;
    fn load_state(&self, reader: &dyn BackendReader) -> Result<(), PipelineError>;
}
```

Two bridge types exist:

- **`LocalBridge<I, S>`** — handles all BlockLocal compositions via
  `MergeStrategy<I>` dispatch (AppendStrategy, MonoidalStrategy, FoldStrategy).
- **`CumulativeBridge<I, S>`** — handles SelfCumulative compositions. Maintains
  `running_state` across blocks; loads prior state from backend on resume.

`MergeStrategy<I>` is a sealed trait that unifies the three composition
algebras into a single `accumulate_one` / `merge_deltas` interface, so the
bridge code doesn't branch on composition type.

CrossIndex bridges are **not yet implemented** (no real Zcash indexes need
X-scope currently).

---

## 5. Layer 2: Sync Engine

### 5.1 Registration and DAG Construction

At startup, the engine:

1. Accepts index registrations via `IndexSet<Ctx>` builder pattern
   (`.with::<I>()` calls; type-checked at registration).
2. Builds the dependency DAG from declared dependencies (Kahn's topo sort).
3. Validates acyclicity, uniqueness, dep existence (rejects at `.build()` if
   ill-formed).
4. Computes per-edge `FiringRule` (Pipelined or Barrier) and phase assignment.
5. Constructs bridges (LocalBridge / CumulativeBridge) and wraps them as
   `Box<dyn IndexPipeline<Ctx>>`.

### 5.1.1 Scheduler and BatchHandle

The scheduler (`Scheduler`) is the brain of the engine. It tracks per-index
progress and emits only work that is safe to run:

```rust
enum Task {
    Extract(ExtractJob),
    CompleteBatch(BatchHandle<FullyExtracted>),
}
```

State transitions are phantom-typed to prevent out-of-order reporting:

```rust
struct FullyExtracted(()); // private unit — can't be constructed externally
struct Merged(());

struct BatchHandle<State> {
    index: IndexId,
    batch: BatchIndex,
    _state: PhantomData<State>,
}
```

Lifecycle: `extraction_done()` -> `BatchHandle<FullyExtracted>` ->
`merge_done()` -> `BatchHandle<Merged>` -> `batch_committed()`. Each
transition is a method that consumes the handle and returns the next state.

### 5.2 Runtime Pipeline

The implementation uses a supply/demand loop rather than the originally
envisioned streaming-channel architecture. The engine is `SyncEngine<Ctx, B>`
with three entry points:

```rust
// Pre-loaded blocks (tests, small ranges)
fn sync_range(&mut self, blocks: Vec<Ctx>) -> Result<(), SyncError>;

// Lazy iterator (streaming without async)
fn sync_streaming<I: IntoIterator<Item = Ctx>>(&mut self, source: I)
    -> Result<(), SyncError>;

// Async channel (production: provisioner on separate task)
async fn sync_channel(&mut self, rx: tokio::sync::mpsc::Receiver<Ctx>)
    -> Result<(), SyncError>;
```

The core loop (`sync_streaming`) is a three-phase dispatch:

1. **Supply**: pull up to `batch_size` blocks from source into `BlockBuffer`.
2. **Demand**: call `scheduler.ready_work()` → dispatch tasks (extract via
   rayon `par_iter`, then merge/persist/commit).
3. **Evict**: after all indexes commit batch N, evict buffer entries through N.

```
┌──────────────┐     ┌──────────────────┐     ┌────────────────┐
│ Source        │────→│ BlockBuffer      │────→│ Engine loop     │
│ (iter/channel)│     │ (sliding window) │     │ supply/demand   │
└──────────────┘     └──────────────────┘     └───────┬────────┘
                                                       │ batch ready
                                              ┌────────▼────────┐
                                              │ Scheduler       │
                                              │ ready_work()    │
                                              └────────┬────────┘
                                                       │ Task
                                              ┌────────▼────────┐
                                              │ Extract (rayon)  │
                                              │ Merge + Persist  │
                                              │ Commit + Evict   │
                                              └─────────────────┘
```

**BlockBuffer** is a sliding-window `BTreeMap<u32, Arc<Ctx>>` with batch-level
eviction. Backpressure is implicit: when the buffer is full, the supply phase
stalls. One block context serves all indexes (projected via `ProvideContext`).

The scheduler does **not** know about fetching — it only checks if blocks are
available and if dependency firing rules are satisfied.

### 5.3 Batch Size

$B$ controls:
- Flush amortisation (fewer flushes = less I/O overhead).
- Memory pressure (larger $B$ = more deltas in flight).
- Phase gate latency (downstream phases wait for batch-sized chunks).
- $\mathsf{M}$-type collision containment scope.

$B$ does NOT control provisioning or extraction cadence — those stream continuously.

### 5.4 Phase Execution

For multi-phase DAGs, the engine runs phases with staggered batch pipelining
where dependency read patterns allow it:

```
Phase 0: [extract β₀][extract β₁][extract β₂] ...
              ↓ commit
Phase 1:          [extract β₀][extract β₁] ...
                       ↓ commit
Phase 2:                   [extract β₀] ...
```

Each phase trails the one above by one batch boundary. In steady state, all
phases are active simultaneously. The bottleneck phase determines throughput.

For dependencies that require global/final state (`SourceAccess::NonLocal` or
specific read patterns), the engine waits for the dependency to complete the
entire chain before starting the downstream phase.

### 5.5 Progress and Crash Recovery

**Implemented.** The engine stores a watermark (`METADATA_INDEX` / `WATERMARK_KEY`)
atomically with each batch commit in `try_commit()`. On startup,
`SyncEngine::committed_height(&backend)` reads the watermark to determine
resume height. The `SyncProgress` struct tracks the watermark in memory.

Partially committed batches are discarded (the backend's atomic commit
guarantees no partial state). For S-scope indexes, the `CumulativeBridge`
loads its prior accumulated state from the backend via
`IndexPipeline::load_state(reader)` on resume.

### 5.6 Tracing

**Implemented** behind a non-default `tracing` feature flag (per privacy
policy — observability must be opt-in). Instrumented spans cover:
`sync_range`, `sync_streaming`, `sync_channel`, `dispatch_tasks`,
`run_extractions_parallel`, `merge_persist`, `try_commit`.

---

## 6. Layer 3: Provisioner

### 6.1 Trait

Current implementation is a synchronous MVP:

```rust
trait Provisioner: Send + Sync {
    type BlockContext: Send + Sync;

    /// Fetch a contiguous range of block contexts.
    fn provision_range(
        &self,
        from: BlockHeight,
        to: BlockHeight,
    ) -> Result<Vec<Self::BlockContext>, ProvisionError>;
}
```

The engine uses this via `sync_range` / `sync_streaming` (synchronous), or
spawns the provisioner on a separate tokio task feeding an `mpsc` channel
consumed by `sync_channel` (async production path).

> **True north**: the provisioner should be a streaming async source with
> configurable concurrency and backpressure from the engine's `BlockBuffer`.
> The `sync_channel` entry point is the stepping stone. The `provision_range`
> → `Vec` shape is retained for tests and small ranges.

### 6.2 Zcash Implementation (not yet started)

```rust
struct ZcashProvisioner {
    source: Arc<dyn BlockchainSource>,
    network: Network,
    concurrency: usize,
}

struct ZcashBlockContext {
    height: Height,
    hash: BlockHash,
    header: BlockHeader,
    transactions: Vec<Transaction>,
    tree_roots: Option<(SaplingRoot, OrchardRoot)>,
    tree_sizes: Option<(u32, u32)>,
    parent_chainwork: Option<ChainWork>,
}
```

Context projection (`ProvideContext`) replaces the `SourceRequirements`
bitflag approach — the provisioner always fetches the full context; each
index receives its projection. See § 4.2.

---

## 7. Layer 4: Backend

### 7.1 Trait

```rust
trait Backend: Send + Sync {
    type Reader: BackendReader;
    type Writer: BackendWriter;

    fn reader(&self) -> Result<Self::Reader, BackendError>;
    fn writer(&self) -> Result<Self::Writer, BackendError>;
    fn flush(&self) -> Result<(), BackendError>;
    fn topology(&self) -> WriterTopology;
}

enum WriterTopology {
    /// All indexes share a single writer. Writes are serialised.
    SharedWriter,
    /// Each index has its own writer. Writes parallelise.
    PerIndexWriter,
}

trait BackendWriter: Send {
    /// Commit a batch of write operations atomically.
    fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), BackendError>;
}

trait BackendReader: Send {
    fn get(&self, index: IndexId, key: &[u8]) -> Result<Option<Vec<u8>>, BackendError>;
    fn scan(&self, index: IndexId) -> Result<Vec<(Vec<u8>, Vec<u8>)>, BackendError>;
}
```

> **DepsReader**: the design specified a visibility-restricted wrapper around
> `BackendReader`. In the implementation, `DepsReader` is a **placeholder
> struct** (no methods). It is passed to `ExtractCross` but cannot yet
> provide data. This is acceptable because no real Zcash indexes use X-scope.
> When CrossIndex bridges are built, `DepsReader` will wrap `BackendReader`
> with a `visible: HashSet<IndexId>` filter as originally designed.

> **BackendReader::scan** replaces `cursor()`. Used by `CumulativeBridge` to
> load prior state on resume via `IndexPipeline::load_state(reader)`.

### 7.2 WriteOp

```rust
enum WriteOp {
    Put {
        index: IndexId,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        index: IndexId,
        key: Vec<u8>,
    },
}
```

Note: `IndexId` (a newtype) replaces `&'static str` for index names.

---

## 8. Coexistence with Existing Code

Both old and new write paths implement a shared strategy trait:

```rust
trait SyncStrategy: Send + Sync {
    async fn sync_to_height(
        &self,
        target: Height,
        source: &dyn BlockchainSource,
    ) -> Result<()>;
}
```

- `MonolithicSync`: wraps the existing `write_block_batch_blocking` path. Zero
  changes to existing code.
- `DagSync`: wires the sync engine with registered indexes, provisioner, and
  backend.

Selection is config-driven. Both produce the same LMDB tables (assuming the same
index set), so the read-side traits are unaffected.

---

## 9. The Implementor's Experience

Adding a new (L,A) index:

```rust
struct TransparentSpenderIndex;

impl IndexDef for TransparentSpenderIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<(Outpoint, SpenderRef)>;
    type BlockContext = ZcashBlockContext;

    const NAME: IndexId = IndexId::new("transparent_spender");
}

impl ExtractLocal for TransparentSpenderIndex {
    fn extract(ctx: &ZcashBlockContext) -> Result<Self::Delta, ExtractError> {
        let mut entries = Vec::new();
        for tx in &ctx.transactions {
            let txid = tx.txid();
            for input in tx.transparent_inputs() {
                if let Some(outpoint) = input.outpoint() {
                    entries.push((outpoint, SpenderRef { txid, .. }));
                }
            }
        }
        Ok(entries)
    }
}

impl MergeAppend for TransparentSpenderIndex {}  // marker only

impl Schema<Vec<Self::Delta>> for TransparentSpenderIndex {
    type Key = Outpoint;
    type Value = SpenderRef;

    fn into_entries(deltas: Vec<Self::Delta>) -> Vec<(Outpoint, SpenderRef)> {
        deltas.into_iter().flatten().collect()
    }
    fn from_entries(entries: Vec<(Outpoint, SpenderRef)>) -> Vec<Self::Delta> {
        vec![entries]
    }
}
```

The implementor:
- Declares scope and composition **in the type system** (not runtime enums).
- Implements extraction with a scope-appropriate signature (no unused params).
- Implements `Schema` for typed persistence (serialisation is separate via `Encode`).

The implementor does NOT:
- Think about scheduling, parallelism, or phase ordering.
- Manage transactions, commits, or flushes.
- Know about other indexes or the provisioner's internals.
- Write any concurrency code.
- Produce `WriteOp`s directly — the bridge handles that via `Schema` + `Encode`.

---

## 10. Open Questions

- **Validation**: should block validation (parent-hash continuity, merkle root)
  be a responsibility of the engine, the provisioner, or a dedicated "validation
  index" that produces no write ops but can fail the batch?

- **Reorg handling**: how does the engine handle chain reorganisations during
  tip-following? The model covers initial sync; steady-state tip-following with
  rollbacks has different characteristics.

- **Dynamic index sets**: can indexes be added or removed at runtime (e.g.,
  feature-gated indexes enabled by config), or is the index set fixed at startup?

- **Incremental provisioner**: during tip-following (post initial sync), the
  provisioner streams one block at a time rather than batch ranges. Should the
  engine have a separate tip-following mode, or does the same pipeline work with
  $B = 1$?

## 11. Implementation Status

Summary of what exists in `zaino-sync` (`feature/sync-engine-draft` branch)
as of 2026-07-09:

### Done

- Full trait hierarchy: `IndexDef`, scope-specific extract traits, composition-
  specific merge traits, `Schema<M>`, `Encode`/`Decode`
- Type-level descriptors with sealed Scope/Composition markers + runtime mirrors
- `DependencyDag` with Kahn's topo sort, `FiringRule` (Pipelined/Barrier)
- `Scheduler` with phantom-typed `BatchHandle` lifecycle
- `BlockBuffer` (sliding window, batch eviction)
- `LocalBridge` (all L×{A,M,F} compositions) via `MergeStrategy` dispatch
- `CumulativeBridge` (S×{M,F}) with `running_state` threading
- `IndexSet` builder (`.with::<I>()` + `.build()`)
- `SyncEngine` with three entry points (`sync_range`, `sync_streaming`,
  `sync_channel`)
- Watermark persistence and crash resume
- Feature-gated tracing
- `InMemoryBackend`, `SlowBackend`, `MockProvisioner`, 4 toy indexes, 29 tests

### Not Done

- **CrossIndex bridges**: `DepsReader` is a placeholder; `ExtractCross` compiles
  but cannot provide data; Barrier firing rule always blocks (scheduler TODO)
- **SourceHandle / NonLocalSource**: placeholder structs only
- **Streaming provisioner**: `provision_range -> Vec` is sync; true streaming
  awaits real provisioner implementation
- **Real Zcash indexes**: ~10 indexes, not started
- **Real provisioner**: Zebra ReadState / JSON-RPC adapter, not started
- **Real backend**: LMDB adapter, not started
- **zainod wiring**: IndexSet registration at startup, not started
