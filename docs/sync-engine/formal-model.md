# Index Sync Model

A formal model for reasoning about parallel index construction in blockchain indexers.

## 1. Motivation

A blockchain indexer maintains a set of **indexes** — persistent data structures that
answer queries about the chain. During initial sync the indexer must construct all
indexes from genesis to tip. The order and degree of parallelism with which indexes
can be built is not arbitrary: it is constrained by data dependencies between indexes,
by the algebraic structure of their update operations, and by the cost model of the
persistence layer.

This document formalises those constraints into a model that is:

1. **Declarative** — each index is described by a small tuple of properties.
2. **Compositional** — the initial sync schedule for an arbitrary set of indexes is
   derived mechanically from their individual descriptors.
3. **Implementation-agnostic** — the model separates *what* constraints exist from
   *how* a particular engine satisfies them, enabling the invariants to be enforced
   by a framework rather than by developer discipline.

---

## 2. Definitions

### 2.1 Chain and Blocks

A **chain** $\mathcal{C}$ is an ordered sequence of **blocks**
$\langle b_1, b_2, \ldots, b_N \rangle$ where each $b_i$ is a self-contained
data payload at **height** $i$. Blocks are the atomic unit of consensus; they
arrive from a **source** and cannot be subdivided at the fetch boundary.

Within a block, a sequence of **transactions** $\langle t_1, \ldots, t_k \rangle$
may provide a finer-grained unit of independent work for some operations.

### 2.2 Index

An **index** $I$ is a persistent map $K_I \to V_I$ that is derived deterministically
from the chain. Given the same chain, the same index contents must result.

An index **entry** is a single $(k, v)$ pair written to the index during construction.

### 2.3 Index Set

An **index set** $\mathcal{I} = \{I_1, I_2, \ldots, I_n\}$ is the complete
collection of indexes that the indexer maintains. The sync problem is: given a chain
$\mathcal{C}$ and an index set $\mathcal{I}$, construct all indexes correctly and as
fast as possible.

### 2.4 Terminology

Several terms in this domain are overloaded. This document uses the following
conventions consistently:

| Term | Meaning |
|------|---------|
| **Initial sync** | The whole process of constructing all indexes from genesis to chain tip. The problem this model addresses. |
| **Coordination** | Worker-to-worker synchronisation: barriers, ordering constraints, and readiness conditions that ensure indexes are built in a valid order. |
| **Flush** | Forcing durably-committed data to stable storage (i.e., `fsync`). Provides the guarantee that persisted data survives process or power failure. |
| **Commit** | Making a batch of writes visible to readers within the persistence layer. After commit, the data is queryable; after flush, it is durable. A commit without a subsequent flush is visible but not crash-safe. |
| **Merge** | Combining per-block contributions into a batch-level or index-level result, according to the index's composition type ($\mathsf{A}$, $\mathsf{M}$, or $\mathsf{F}$). |
| **Extraction** | Computing the per-block contribution $\delta_I(b)$ for a given index $I$ and block $b$. The CPU-bound work of parsing, encoding, and checksumming. |
| **Pipelining** | Overlapping the extraction of block $b_{h+1}$ with the merge or fold of block $b_h$. Pipelining does not violate sequential dependencies — it exploits the fact that extraction and merging are different operations that may run concurrently when one does not depend on the other's result. |

---

## 3. The Two Axes

Every index has two orthogonal properties that determine its initial sync behaviour.
Together they form a classification grid that governs phase placement, parallelism,
and merge strategy.

### 3.1 Axis 1: Input Scope

**Input scope** answers: *what data must be available before computing index $I$'s
entries for block $b_h$?*

| Symbol | Name | Definition |
|--------|------|------------|
| $\mathsf{L}$ | **Block-local** | Only the raw bytes of $b_h$ are needed. No dependency on any prior block or other index. |
| $\mathsf{S}$ | **Self-cumulative** | The index's own accumulated state through height $h{-}1$ is needed. The computation at height $h$ depends on the result at height $h{-}1$. |
| $\mathsf{X}(D)$ | **Cross-index** | The output of one or more other indexes $D \subseteq \mathcal{I} \setminus \{I\}$ is needed. |

These can combine: an index may be both $\mathsf{S}$ and $\mathsf{X}(D)$, meaning it
depends on its own prior state *and* on other indexes.

**Implications for ordering:**

- $\mathsf{L}$: No ordering constraints from input scope. Can begin immediately.
- $\mathsf{S}$: Creates an intra-index ordering. Block $h$ cannot be processed
  before block $h{-}1$ for this index (though this constraint may be relaxable via
  axis 2; see below).
- $\mathsf{X}(D)$: Creates an inter-index ordering. Index $I$ cannot begin (or at
  least cannot complete) until every $J \in D$ is available for reading.

### 3.2 Axis 2: Composition Type

**Composition type** answers: *given per-block (or per-batch) contributions computed
independently, how are they combined into the final index state?*

| Symbol | Name | Definition | Parallelisation strategy |
|--------|------|------------|--------------------------|
| $\mathsf{A}$ | **Append** | Each block writes to disjoint keys. Contributions from different blocks never collide. | Embarrassingly parallel. Merge = concatenation. |
| $\mathsf{M}$ | **Monoidal** | Keys may overlap, but the merge operation forms a commutative monoid $(V_I, \oplus, e)$ — i.e., $\oplus$ is associative and commutative with identity $e$. | Parallel map-reduce. Batch results can be merged in any order. |
| $\mathsf{F}$ | **Fold** | The merge is order-dependent. The contribution of block $h$ to the final state depends on the *sequence* of prior contributions, not just their aggregate. | Sequential merge only. Can be pipelined (see below) but not parallelised across blocks. |

**Pipelining in $\mathsf{F}$-type indexes:**

For an $(\mathsf{S}, \mathsf{F})$ index, processing a single block has two stages:

1. **Extract**: parse the raw block bytes into a contribution $\delta_I(b_h)$. This
   only needs block data, not prior index state.
2. **Fold**: apply $f(s_{h-1}, \delta_I(b_h)) \to s_h$. This needs the previous
   state and therefore cannot run until the prior fold completes.

The fold is sequential, but extraction is not. Pipelining overlaps the extraction of
future blocks with the fold of the current block:

```
Extract:  [b₁][b₂][b₃][b₄][b₅]...     ← can run ahead
Fold:         [b₁][b₂][b₃][b₄]...      ← strictly sequential, stalls until prior fold completes
```

Without pipelining, each block waits for both extraction and fold to complete before
the next block's extraction begins. Pipelining hides extraction latency behind fold
latency (or vice versa), improving throughput by the ratio of the two.

The key distinction from $(\mathsf{S}, \mathsf{M})$ is: a monoidal accumulation can
use parallel prefix to compute **all** cumulative states simultaneously. A
non-decomposable fold genuinely must apply $f$ in sequence — pipelining is the
strongest available strategy.

**Interaction with self-cumulative scope ($\mathsf{S}$):**

An index that is $(\mathsf{S}, \mathsf{M})$ has a self-referential dependency, but
the accumulation operation is monoidal. This means the sequential dependency is
*algebraically decomposable*: parallel prefix algorithms can compute all $N$
cumulative values in $O(N / P + \log P)$ time on $P$ processors. The canonical
example is a running sum: $s_h = s_{h-1} + \delta_h$ is sequential, but addition is
associative, so prefix sums parallelise.

### 3.3 Classification Grid

The two axes form a grid. Each cell has a characteristic strategy:

|  | $\mathsf{A}$ (append) | $\mathsf{M}$ (monoidal) | $\mathsf{F}$ (fold) |
|---|---|---|---|
| $\mathsf{L}$ (block-local) | Embarrassingly parallel. No coordination needed. | Parallel map, reduce at merge. | Parallel extraction, sequential merge. |
| $\mathsf{S}$ (self-cumulative) | Pipeline, or parallel prefix if the accumulation is monoidal. | Parallel prefix. | Strictly sequential merge. Pipelining only. |
| $\mathsf{X}(D)$ (cross-index) | Dependency gate, then embarrassingly parallel. | Dependency gate, then map-reduce. | Dependency gate, then sequential merge. Worst case. |

### 3.4 Relationship to the "All-Isolated" Model

A simpler model sometimes proposed is to require that every index be buildable in
complete isolation — no index may depend on any other index or on its own prior
state. This model is a **special case** of the one presented here: it restricts
every index to $\sigma_I = \mathsf{L}$ and $D_I = \emptyset$.

Under that restriction:
- All indexes are in phase 0 (no DAG edges).
- The classification grid collapses to a single row.
- The only variation is along the composition axis.

This model subsumes the all-isolated model by additionally supporting indexes with
$\sigma = \mathsf{S}$ or $\sigma = \mathsf{X}(D)$, and by deriving the exact
constraints those dependencies introduce rather than prohibiting them outright. The
all-isolated model is sufficient for parallelism but not necessary; this model
captures the weaker, necessary conditions.

---

## 4. Index Descriptor

Each index is fully characterised for initial sync purposes by a **descriptor**:

$$
I = (\sigma_I,\; \gamma_I,\; c_I,\; w_I,\; D_I)
$$

| Component | Type | Meaning |
|-----------|------|---------|
| $\sigma_I$ | $\in \{\mathsf{L},\; \mathsf{S},\; \mathsf{X}\}$ | Input scope |
| $\gamma_I$ | $\in \{\mathsf{A},\; \mathsf{M},\; \mathsf{F}\}$ | Composition type |
| $c_I : \mathbb{B} \to \mathbb{R}^+$ | function | Compute cost per block (may depend on block content: tx count, action count, etc.) |
| $w_I : \mathbb{B} \to \mathbb{N}$ | function | Write amplification: number of entries produced per block |
| $D_I$ | $\subseteq \mathcal{I} \setminus \{I\}$ | Dependency set (empty if $\sigma_I = \mathsf{L}$; must be non-empty if $\sigma_I = \mathsf{X}$) |

For $\mathsf{M}$-type indexes, the descriptor also implicitly specifies the monoid
$(V_I, \oplus_I, e_I)$. For $\mathsf{S}$-type indexes, it specifies the accumulation
function $\text{acc}_I : S_I \times \Delta_I \to S_I$.

---

## 5. Dependency DAG

Given an index set $\mathcal{I}$ with descriptors, the **dependency graph**
$G = (\mathcal{I}, E)$ has edges:

$$
E = \{(J, I) \mid J \in D_I\}
$$

That is, an edge from $J$ to $I$ means "$I$ depends on $J$."

**Constraint**: $G$ must be a DAG. Cycles would mean deadlock; the index set is
ill-formed if any cycle exists.

### 5.1 Phase Assignment (Conservative Scheduling)

The **phase assignment** $\phi : \mathcal{I} \to \mathbb{N}$ is the topological
layer function:

$$
\phi(I) = \begin{cases}
0 & \text{if } D_I = \emptyset \\
1 + \max_{J \in D_I} \phi(J) & \text{otherwise}
\end{cases}
$$

All indexes in the same phase are mutually independent and can be built concurrently.
Under conservative scheduling, phase $p$ cannot begin until all indexes in phases
$0, \ldots, p{-}1$ have completed their merge for the current batch and their state
is readable.

The **depth** of the DAG — $\max_I \phi(I)$ — is the minimum number of sequential
phase boundaries under this conservative strategy.

### 5.2 Per-Edge Scheduling (Optimal)

Phase assignment groups indexes into uniform layers and places a barrier between each
layer. This over-coordinates when indexes within a phase have different costs or
serve different downstream consumers.

The optimal scheduling strategy operates at the granularity of individual DAG edges
rather than layers. Each index $I$ has a **firing rule**: $I$ can begin processing
batch $\beta_j$ as soon as every dependency in $D_I$ has individually completed the
work required for $\beta_j$.

$$
\text{ready}(I, \beta_j) = \bigwedge_{J \in D_I} \text{available}(J, \beta_j)
$$

Under per-edge scheduling:
- If index $A$ finishes batch $\beta_j$ before unrelated index $B$ (both in the
  same phase), then index $C$ (which depends only on $A$) can start $\beta_j$
  immediately — it need not wait for $B$.
- No artificial grouping into layers is needed; the DAG edges *are* the schedule.

Phase assignment remains useful as a mental model and as a simpler (if conservative)
implementation strategy. Per-edge scheduling is the theoretical optimum.

### 5.3 Dependency Composition Type and Downstream Scheduling

Whether a downstream index can begin consuming a dependency's output before the
dependency has completed its merge for a batch depends on the dependency's
**composition type**:

| Dependency's $\gamma$ | When downstream can read | Why |
|---|---|---|
| $\mathsf{A}$ | Per-entry, as each entry is written | Every entry is final on write. No merge step produces intermediate state. |
| $\mathsf{M}$ | After the batch merge completes | Keys are in flux until the monoidal reduce finishes; reading mid-merge yields an intermediate value that does not correspond to any valid chain state. |
| $\mathsf{F}$ | After the batch fold completes | Fold state is meaningless until applied in full sequence. |

For $\mathsf{A}$-type dependencies, a downstream index could in principle stream
entries from its dependency as they are produced, overlapping extraction and
downstream computation within the same batch. This is a second-order optimisation
that the model notes as sound but does not require.

---

## 6. Batch Processing

### 6.1 Batches

A **batch** $\beta = \langle b_j, b_{j+1}, \ldots, b_{j+B-1} \rangle$ is a
contiguous subsequence of $B$ blocks. The chain is partitioned into
$\lceil N / B \rceil$ batches, where $B$ is the **batch size** — a tuning parameter.

Within a batch, per-block contributions are computed (potentially in parallel) and
then merged and persisted together.

Batches serve a dual role:

1. **Performance**: amortise flush cost over $B$ blocks rather than paying it per
   block.
2. **Correctness**: contain $\mathsf{M}$-type key collisions to a controlled,
   single-writer merge scope. Without batching, concurrent writes to overlapping
   keys would require per-key locking or atomic read-modify-write support in the
   persistence layer.

### 6.2 Processing a Batch

For a single phase $p$ and batch $\beta$:

**Step 1: Extract.** For each index $I$ with $\phi(I) = p$ and each block
$b \in \beta$, compute the per-block contribution $\delta_I(b)$.

- If $\sigma_I = \mathsf{L}$: each $\delta_I(b)$ can be computed independently.
- If $\sigma_I = \mathsf{S}$: contributions within the batch may require
  intra-batch sequencing (or parallel prefix if $\gamma_I = \mathsf{M}$).
- If $\sigma_I = \mathsf{X}$: contributions may require reads from indexes
  whose dependency gate has been satisfied in earlier phases.

**Step 2: Merge.** Combine per-block contributions into a batch-level result.

- If $\gamma_I = \mathsf{A}$: the batch result is the union of all entries
  (disjoint keys; no conflict resolution needed).
- If $\gamma_I = \mathsf{M}$: the batch result is
  $\bigoplus_{b \in \beta} \delta_I(b)$, computed via the monoidal operation.
  Commutativity and associativity guarantee this can be done in any order.
- If $\gamma_I = \mathsf{F}$: the batch result must be computed by applying
  contributions in chain order.

**Step 3: Commit.** Write the batch-level results to the persistence layer and make
them visible to readers. After commit, subsequent phases (or the next batch of the
current phase) can read the committed entries.

**Step 4: Flush.** Force the committed data to stable storage (`fsync`). After
flush, the data is durable — it will survive process or power failure. Flush is the
expensive I/O operation; its cost is amortised over all $B$ blocks in the batch.

Commit and flush are distinct operations with different costs and guarantees:

| Operation | After completion | Cost |
|-----------|-----------------|------|
| **Commit** | Data is visible to readers within the persistence layer | Fast (in-memory page table update) |
| **Flush** | Data is durable on stable storage | Slow (device I/O, typically 1-10ms on SSD) |

A batch may be committed (enabling downstream phases to proceed) before it is
flushed, if the system tolerates replaying uncommitted batches on crash recovery.
Alternatively, commit and flush may be combined as a single atomic step for
simplicity.

### 6.3 Cross-Batch Dependencies

For $(\mathsf{S}, \mathsf{M})$ indexes, cross-batch state threading can use
**parallel prefix**:

1. Each batch computes its local aggregate $a_\beta = \bigoplus_{b \in \beta} \delta_I(b)$.
2. A prefix scan over the batch aggregates produces cumulative state at each batch boundary.
3. Each batch can then derive per-block cumulative values from its local data and the prefix result.

This reduces the sequential dependency from $N$ blocks to $\lceil N/B \rceil$ batch-level prefix operations.

For $(\mathsf{S}, \mathsf{F})$ indexes, no such decomposition exists. The batch must
receive the prior batch's final state before its fold step can proceed (though its
extraction step can run ahead; see Section 3.2 on pipelining). This makes
$(\mathsf{S}, \mathsf{F})$ the critical path bottleneck for any initial sync
pipeline.

### 6.4 Cross-Phase Pipelining

Phases do not need to wait for the previous phase to complete across the entire
chain before starting. A downstream phase can begin processing batch $\beta_j$ as
soon as the dependency gate for $\beta_j$ is satisfied (per Section 5.2).

For dependencies with read pattern $\mathsf{R_{\leq}}$ (the downstream index at
height $h$ reads from its dependency at heights $\leq h$), pipelining applies:
phase $p$ can process batch $\beta_j$ while phase $p-1$ processes $\beta_{j+1}$.
In steady state, all phases are active simultaneously, staggered by one batch.

For dependencies that require the dependency's global/final state
($\mathsf{R_{*}}$), pipelining is not possible: the downstream phase must wait for
the dependency to complete the entire chain.

The read pattern is a property of the specific dependency relationship, not of the
index in isolation. Each entry in $D_I$ should be understood as carrying an implicit
read pattern.

---

## 7. Cost Model

### 7.1 Per-Batch Cost

For a single batch $\beta$ in phase $p$:

$$
T_{\text{batch}}(p, \beta) = T_{\text{compute}}(p, \beta) + T_{\text{write}}(p, \beta) + T_{\text{flush}}
$$

**Compute:**

$$
T_{\text{compute}}(p, \beta) = \max_{I : \phi(I) = p} \left[
  \frac{1}{P_I} \sum_{b \in \beta} c_I(b)
  + R_I(\beta)
  + M_I(\beta)
\right]
$$

- $P_I$: number of parallel workers available for index $I$'s extraction
- $R_I(\beta)$: cost of reading from dependency indexes for this batch
- $M_I(\beta)$: cost of merging per-block contributions within this batch

The $\max$ arises because indexes within a phase are concurrent; the slowest
determines the phase's compute time.

**Write:**

Under a **shared-writer** persistence model (e.g., single LMDB environment):

$$
T_{\text{write}}(p, \beta) = \sum_{I : \phi(I) = p} w_I(\beta) \cdot \omega
$$

where $\omega$ is the amortised cost per `put` operation and $w_I(\beta) = \sum_{b \in \beta} w_I(b)$.

Under a **per-index-writer** model (separate environments):

$$
T_{\text{write}}(p, \beta) = \max_{I : \phi(I) = p}\; w_I(\beta) \cdot \omega
$$

**Flush:**

$$
T_{\text{flush}} = F
$$

A largely fixed cost per batch. Typically 1-10 ms on SSD, ~50 ms on spinning disk.

### 7.2 Total Initial Sync Time

$$
T_{\text{total}} = \sum_{p=0}^{\text{depth}} \sum_{j=0}^{\lceil N/B \rceil - 1}
T_{\text{batch}}(p, \beta_j)
$$

Note that the sum over phases is sequential (phase boundaries require coordination),
while within each phase, batches may overlap via pipelining if sufficient parallelism
exists. With cross-phase pipelining (Section 6.4), the phases overlap as well,
reducing total time toward:

$$
T_{\text{pipelined}} \approx \text{depth} \times T_{\text{batch,max}} + \frac{N}{B} \times \max_{p}\; T_{\text{batch}}(p)
$$

where the first term is pipeline fill latency and the second is steady-state
throughput governed by the bottleneck phase.

### 7.3 Marginal Cost of a New Index

Given an existing index set $\mathcal{I}$ and a proposed new index $I_{\text{new}}$
with descriptor $(\sigma, \gamma, c, w, D)$:

**Phase placement**: $\phi(I_{\text{new}})$ is determined by $D$ as per Section 5.

**Case 1** — $I_{\text{new}}$ joins an existing phase $p$:

$$
\Delta T_{\text{compute}} = \sum_\beta \max\!\Big(0,\;
  T_{I_{\text{new}}}(\beta) - \max_{I : \phi(I) = p} T_I(\beta)
\Big)
$$

If $I_{\text{new}}$ is faster than the current bottleneck in phase $p$, its compute
cost is fully hidden: $\Delta T_{\text{compute}} = 0$.

$$
\Delta T_{\text{write}} = \begin{cases}
\sum_\beta w_{I_{\text{new}}}(\beta) \cdot \omega & \text{(shared writer)} \\
\sum_\beta \max(0,\; w_{I_{\text{new}}}(\beta) \cdot \omega - W_{\max}(p, \beta)) & \text{(per-index writer)}
\end{cases}
$$

**Case 2** — $I_{\text{new}}$ creates a new phase $p' = \text{depth} + 1$:

$$
\Delta T = \sum_\beta \Big[ T_{I_{\text{new}}}(\beta) + w_{I_{\text{new}}}(\beta) \cdot \omega + F \Big]
$$

The full cost is additive — nothing hides behind it.

---

## 8. Invariants

The following properties must hold for any correct initial sync execution. These are
the rules that a framework can enforce mechanically.

### 8.1 Dependency Precedence

> **Invariant 1.** For every index $I$ and every $J \in D_I$: all entries of $J$
> through height $h$ must be readable before computing $I$'s entries at height $h$.

This is enforced by the firing rules described in Section 5.2: either via
conservative phase barriers or via per-edge readiness tracking.

### 8.2 Merge Determinism

> **Invariant 2.** For $\mathsf{A}$-type indexes: keys produced by different blocks
> must be disjoint. For $\mathsf{M}$-type indexes: the merge operation must be
> associative and commutative. For $\mathsf{F}$-type indexes: contributions must be
> applied in strict chain order.

This is enforceable at the type level: the framework can require $\mathsf{M}$-type
indexes to provide a monoid implementation that the merge step uses, and can enforce
ordering for $\mathsf{F}$-type merges.

### 8.3 Batch Atomicity

> **Invariant 3.** All entries produced by a batch are committed atomically. Either
> all entries for all indexes in the batch are committed, or none are.

This ensures crash recovery is clean: the indexer can resume from the last
fully-committed batch boundary without partial state.

### 8.4 DAG Well-Formedness

> **Invariant 4.** The dependency graph $G$ induced by the descriptors $\{D_I\}$
> must be acyclic. This is a static property of the index set and can be verified
> at registration time.

---

## 9. Separation of Concerns

The central question: can the model's invariants be enforced by a generic framework,
so that *implementing a new index* does not require understanding the full initial
sync theory?

The answer is yes, via a clean separation into three layers.

### 9.1 Layer 1: Index Definition (provided by the implementor)

For each index, the implementor provides:

1. **Descriptor declaration**: the tuple $(\sigma, \gamma, D)$ — input scope,
   composition type, and dependency set. These are static, declarative properties.

2. **Extract function**: given a block (and, if $\sigma = \mathsf{X}$, a read handle
   to dependency indexes), produce the per-block contribution $\delta_I(b)$.

3. **Merge specification** (required only if $\gamma \neq \mathsf{A}$):
   - For $\mathsf{M}$: the monoid $(V_I, \oplus, e)$.
   - For $\mathsf{F}$: the fold function $f : S \times \delta \to S$.

4. **Write function**: given the merged batch result, produce the persistence
   operations (key-value puts and deletes).

The implementor does **not** specify:
- When to run relative to other indexes (derived from the DAG).
- How many blocks to process per batch (a tuning parameter).
- When to flush (a framework decision).
- How to parallelise extraction (the framework maps over blocks/txs).

### 9.2 Layer 2: Sync Engine (the framework)

The framework is parameterised by an index set and handles:

1. **DAG construction and validation.** At registration time, read all descriptors,
   build $G$, verify acyclicity (Invariant 4), and compute the phase assignment
   $\phi$.

2. **Batch scheduling.** Partition the chain into batches of size $B$. For each
   phase, dispatch extraction work to a thread pool.

3. **Dependency wiring.** For phase $p > 0$, provide indexes in phase $p$ with
   read handles to the (now-committed) indexes from earlier phases. The framework
   guarantees Invariant 1 by construction — the read handles are not available
   until the dependency's firing rule is satisfied.

4. **Merge dispatch.** After extraction, invoke the appropriate merge strategy
   based on $\gamma$:
   - $\mathsf{A}$: no-op merge (just collect).
   - $\mathsf{M}$: parallel reduce using the declared monoid.
   - $\mathsf{F}$: sequential apply in chain order.
   This enforces Invariant 2.

5. **Persistence and flush.** Collect write operations from all indexes in the
   batch, commit them in a single transaction, then flush. This enforces
   Invariant 3.

6. **Progress tracking.** Maintain a persistent watermark (the last fully-flushed
   batch boundary) for crash recovery.

### 9.3 Layer 3: Persistence Backend (pluggable)

The persistence layer is a separate concern from both the index logic and the sync
engine:

1. **Writer interface**: accepts batched key-value operations (puts, deletes) and
   commits them atomically.
2. **Reader interface**: provides point lookups and cursor scans over committed data.
3. **Flush interface**: exposes durability flush with configurable guarantees.
4. **Topology**: may offer a single shared writer or per-index writers, which the
   sync engine can query to choose its write cost model (sum vs max).

### 9.4 The Implementor's Experience

Under this separation, adding a new index to the system involves:

1. Declare the descriptor: "I am $(\mathsf{X}, \mathsf{M})$ and I depend on
   index $J$."
2. Implement `extract(block, deps) -> entries`.
3. Implement the monoid `merge(a, b) -> c` (if $\mathsf{M}$) or fold (if
   $\mathsf{F}$).
4. Implement `write(result) -> [put/delete ops]`.

The framework handles everything else. Crucially:

- **Phase placement is automatic.** The implementor declares dependencies; the
  framework computes the phase.
- **Parallelism is automatic.** The framework parallelises extraction across
  blocks (and optionally across transactions) without the implementor writing
  any concurrency code.
- **Merge correctness is enforced.** The framework calls the monoid/fold in the
  right pattern; the implementor cannot accidentally apply contributions out of
  order for an $\mathsf{F}$-type index.
- **Persistence atomicity is guaranteed.** The implementor never calls flush or
  manages transactions; the framework batches and commits.

The invariants from Section 8 are structural properties of the framework, not
conventions that implementors must remember.

### 9.5 What This Does Not Capture

The model and framework enforce *ordering and concurrency* invariants. They do not
enforce *semantic* correctness of individual indexes: if an implementor writes a
buggy `extract` function that produces wrong entries, the framework will
faithfully persist them. Semantic correctness remains the implementor's
responsibility, testable through standard unit and integration testing against
known chain data.

The model also does not capture **reorg handling** (rolling back the tip region
when the chain reorganises). Reorgs have different cost characteristics from
initial sync — batch sizes are small (1-2 blocks), and the cost of *undoing*
entries depends on the composition type:
- $\mathsf{A}$: delete the keys written for the reverted block.
- $\mathsf{M}$: apply the inverse operation $\ominus$ if the monoid has
  inverses, or recompute from a checkpoint.
- $\mathsf{F}$: recompute from the last checkpoint.

A full model of reorg cost is left for future work.

---

## 10. Summary

| Concept | What it captures |
|---------|-----------------|
| **Input scope** ($\sigma$) | When an index can start: immediately, after itself, or after dependencies |
| **Composition type** ($\gamma$) | How per-block work combines: trivially, via reduce, or sequentially |
| **Descriptor tuple** | Complete specification of an index's initial sync properties |
| **Dependency DAG** | The minimum sequential structure, derived mechanically from descriptors |
| **Batch size** ($B$) | Amortisation knob for flush cost and collision containment for $\mathsf{M}$-type indexes |
| **Cost model** | Quantitative framework for estimating initial sync time and marginal impact of changes |
| **Three-layer separation** | Ensures the framework enforces invariants; implementors only provide index logic |
| **All-isolated containment** | The all-indexes-independently-buildable model is a special case ($\sigma = \mathsf{L}$ for all $I$) |

The key insight is that the initial sync schedule for any index set is *not a design
decision* — it is a *derivable consequence* of the declared properties of each
index. A framework that computes the schedule from descriptors removes an entire
class of bugs (ordering violations, missed dependencies, incorrect parallelism)
from the implementor's concern.
