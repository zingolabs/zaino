# zaino-sync

The DAG-driven parallel sync engine: it drives index building by pulling blocks
from a source, extracting per-index deltas, merging and persisting them, and
committing each batch atomically to a [`zaino-persistence`] backend. The engine
is generic — it holds no blockchain knowledge; indexes and the provisioner live
in downstream crates.

[`zaino-persistence`]: ../zaino-persistence/usage.md

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
| `merge_persist` | Compact per-index list `id=ms/ops`: merge+persist wall time and the number of write ops that index committed for this batch. |
| `commit_ms` | Wall time of the atomic `writer.commit` (put loop + flush) for this batch. |
| `window_ms` | Wall time since the previous batch commit. |
| `residual_ms` | `window - wait - extract - Σ(this batch's merge_persist) - commit`. |

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
window-globally; `merge_persist` is keyed by the batch each index actually
persisted, so its per-index op counts match exactly what that batch committed.
Because the phases are sequential, `wait + extract + merge_persist + commit`
approximates `window_ms`; `residual_ms` surfaces the difference (merge work done
in this window for a not-yet-committed batch, scheduler overhead, contention).

### Reading it in Loki

```logql
{namespace="<ns>"} |= "sync batch profile"
```

Pair with `|= "lmdb commit split"` to chart `flush_ms` against `wait_ms`: the
former dominating is I/O-bound, the latter dominating is fetch-bound.
