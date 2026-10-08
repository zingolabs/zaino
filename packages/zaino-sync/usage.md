# zaino-sync

The DAG-driven parallel sync engine: it drives index building by pulling blocks
from a source, extracting per-index deltas, merging and persisting them, and
committing each batch atomically to a [`zaino-persistence`] backend. The engine
is generic — it holds no blockchain knowledge; indexes and the provisioner live
in downstream crates.

[`zaino-persistence`]: ../zaino-persistence/usage.md

## Composition merge order

An index declares a composition — `Append`, `Monoidal`, or `Fold` — that says how a
batch's per-block deltas combine. The engine extracts blocks in parallel and out of
chain order, then merges **every** composition in chain order: each delta is tagged
with its block offset and the batch is reordered before the combine runs.

`Monoidal`'s combine must be associative with an identity but is **not** assumed
commutative, and `Fold` is outright order-dependent; neither an index nor an engine
optimisation may rely on a commutative merge. A commutative fast path, if ever
wanted, must be a separately named composition.

## Scope and carry algebra

Orthogonally, an index declares a scope — `BlockLocal`, `SelfCumulative`, or
`CrossIndex` — that says what an extraction needs beyond the current block.
`SelfCumulative` needs the index's own accumulated state from prior blocks, and
is parameterised by a **carry algebra** describing how that state composes:
`SelfCumulative<C: CarryAlgebra = Sequential>`. The parameter defaults to
`Sequential`, so `type Scope = SelfCumulative;` keeps today's block-at-a-time
`extract(ctx, prior)` behaviour. `OrderedMonoid` is the opt-in for a carry that
is an ordered monoid with a measure, letting a batch be built in parallel.

The carry is a parameter of the scope marker precisely because it is meaningful
only for a cumulative scope: `BlockLocal` and `CrossIndex` are not generic over
a carry and cannot name one. The runtime mirror carries it as
`InputScope::SelfCumulative { carry }`; see the `descriptor` module.

## CrossIndex reads: `DepsReader`

A `CrossIndex` index needs another index's output for the blocks it processes.
It declares those indexes in `DEPENDENCIES` and implements `ExtractCross`, whose
`extract(ctx, deps: &DepsReader<'_>)` receives a read handle over its
dependencies. `DepsReader::get::<D>(&key) -> Result<Option<D::Value>,
DepsReadError>` is a typed point read through `D`'s codec; it refuses any index
not in the declared set with `DepsReadError::Undeclared`, and surfaces backend
and decode failures as `DepsReadError::Read` / `Decode` (each a typed `#[source]`
cause). The pair `(CrossIndex, Append)` is served by `CrossBridge`, a
block-parallel bridge like `LocalBridge`.

What a dependency read sees, and when it runs:

- **Committed state overlaid with the dependency's pending batch.** The engine
  commits one atomic transaction per batch across every index plus the
  watermark, so a dependency cannot have *backend-committed* the batch a cross
  index is extracting — the two commit together. `DepsReader` therefore resolves
  a read against the dependency's ops already persisted into the pending atomic
  commit (an in-memory overlay, keyed by namespace and key, latest-wins, a
  delete tombstoning a committed value) layered over the committed backend
  reader. So a cross index reads its dependencies' *same-batch* output.
- **Per-batch gating on the dependency's persist.** A cross index's batch is
  released only once every dependency has persisted that batch (the DAG's
  `Pipelined` firing rule) — the overlay's horizon. Past the gate the batch is
  block-parallel: each block's read is independent, with no inter-block carry.
- **Atomic commit.** The cross index's entries and its dependencies' entries for
  the batch are written in the one atomic transaction.

Note: the `Barrier` firing rule (for a dependency read forward/globally) is not
yet implemented; the scheduler conservatively blocks such an edge. Cross indexes
with a backward (`R≤`) read pattern use `Pipelined`, which is implemented.

## Sync profiling (`sync-profile` feature)

Off by default and compiled out entirely when off (no `Instant::now`, no extra
fields, no allocation). It exists to answer, in process and without perturbing
the run, where each persistence batch's wall time goes — the difference between
an I/O-bound index set and a CPU/fetch-bound one.

Enable it on the binary with `--features zainod/sync-profile` (it forwards to
the engine here and to the LMDB commit split in `zaino-backend-lmdb`). Logs land
wherever `tracing` is configured.

### What it measures

Instrumentation sits only at batch and phase boundaries — never inside
per-block extraction, block intake, or the per-entry put loop. Each phase costs
a few `Instant::now()` calls and `O(1)` length reads per batch.

The engine emits **one event per committed batch**, message `sync batch
profile`, with these fields:

| Field | Meaning |
|---|---|
| `batch` | The committed batch index. |
| `committed_height` | Highest block height this batch made durable (the watermark it stamps). |
| `blocks` | Blocks in the batch. |
| `wait_ms` | Time blocked awaiting blocks from the source — **fetch starvation**. High `wait` means the run is fetch-bound. |
| `extract_ms` | Wall time running per-index extraction across the window. |
| `merge_persist` | Compact per-index list `id=ms/ops`: each index's own merge+persist time and the write ops it committed for this batch. The indexes run in parallel, so **these durations overlap** — read them to see *which* index is slow, never sum them against wall time. |
| `merge_persist_wall_ms` | Wall time of the parallel merge+persist work in the window. This, not the sum of the per-index figures, is the real merge+persist cost. |
| `commit_ms` | Wall time of the atomic `writer.commit` (put loop + flush) for this batch. |
| `window_ms` | Wall time since the previous batch commit. |
| `residual_ms` | `window - wait - extract - merge_persist_wall - commit`. |

The LMDB backend emits a second event per commit, message `lmdb commit split`,
with `put_ms` (building the write txn in memory) and `flush_ms`
(`txn.commit()`), plus `op_count`. The engine opens a `sync_commit` span around
the commit carrying `batch` and `committed_height`, so this split is correlated
to the engine's batch. **`flush_ms` vs `put_ms` is the I/O split**: a large
`flush_ms` is the signature of a write-amplified, I/O-bound index set.

Every 50th commit the backend also emits `lmdb env stats` (B-tree `depth`,
`branch_pages`, `leaf_pages`, `overflow_pages`, `entries`, `page_size`). The
`lmdb` crate exposes only environment-wide stats (the main database), not
per-named-database stats, so these figures are env-wide, not per-index.

### Attribution rule

Phases run sequentially but do not line up one-to-one with commits: a window may
extract blocks for a batch that commits later, and a batch's atomic commit fires
only once every index has persisted it. The rule is: **a phase is attributed to
the batch that commits next.** `wait` and `extract` are accumulated
window-globally; the per-index `merge_persist` samples are keyed by the batch
each index actually persisted, so their op counts match exactly what that batch
committed. Because the phases are sequential, `wait + extract +
merge_persist_wall + commit` approximates `window_ms`; `residual_ms` surfaces
the difference (merge work done in this window for a not-yet-committed batch,
scheduler overhead, contention). `residual_ms` uses `merge_persist_wall_ms` —
the wall time of the parallel merge+persist — not the sum of the overlapping
per-index samples, which would overstate the cost and drive the residual
sharply negative under a multi-index set.

### Reading it in Loki

```logql
{namespace="<ns>"} |= "sync batch profile"
```

Pair with `|= "lmdb commit split"` to chart `flush_ms` against `wait_ms`: the
former dominating is I/O-bound, the latter dominating is fetch-bound.
