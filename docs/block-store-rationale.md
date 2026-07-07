# Block Store Rationale

Why the current `ChainIndex` design is brittle and how the block store fixes it.

## 1. Concurrency

### Current: one mutable structure, many readers

The non-finalized cache is a single `ArcSwap<NonfinalizedBlockCacheSnapshot>`:

```rust
// non_finalised_state.rs:27
current: ArcSwap<NonfinalizedBlockCacheSnapshot>,
```

The writer (sync loop) builds a new snapshot and swaps it in:

```rust
// non_finalised_state.rs:470
working_snapshot: &mut NonfinalizedBlockCacheSnapshot,
// ... mutate in place ...
self.current.store(Arc::new(new_snapshot));
```

Readers call `load_full()` to get an `Arc` bump. This is **wait-free for readers** and
**lock-free for the writer** — the `ArcSwap` itself is sound. The problem is what the
writer does *before* the swap:

```rust
// chain_index.rs:883-900
let intermediate_nfs_for_scoping = nfs.load();
let non_finalized_state = match *intermediate_nfs_for_scoping {
    Some(ref nfs) => nfs,
    None => {
        // Clone the source, call get_block, build initial state...
        nfs.store(Some(Arc::new(
            NonFinalizedState::initialize(source, network, ...).await,
        )));
        &nfs.load_full().expect("just set to Some")
    }
};
// Then sync nfs to chain_height, which mutates and re-stores...
non_finalized_state.sync(fs.clone(), chain_height.into()).await?;
std::mem::drop(intermediate_nfs_for_scoping);
```

There's a `let nfs = self.non_finalized_state.clone()` capture at the top of the
sync loop (line 820), but the code at line 883 loads from `nfs` (the captured
`Arc<ArcSwapOption<...>>`) and then re-stores into it. The `intermediate_nfs_for_scoping`
variable exists because the load and store must not overlap — but they're racing
against the cancellation token and against source-change notifications, all in
the same async task.

**The concurrency model is correct but fragile**: correctness depends on the sync
loop being the *only* writer. If a second writer were introduced (e.g. a background
reorg handler), the `load → mutate → store` sequence would need a compare-and-swap
loop. Today it doesn't have one because there's only one writer. The block store
eliminates this class of bug by making the store append-only: new blocks are
inserted by hash, the height→hash mapping is append-only on the best chain, and
no mutation ever overwrites existing data.

### Block store: append-only, no mutable global state

The formal model in [`BlockStore.lean`](./BlockStore.lean) captures this directly
(Section 6, Thread Safety):

> The ChainStream's correctness depends only on the captured `blocks` and
> `heights` snapshots. The writer's concurrent publish of new roots does not
> affect an existing stream.

A reader captures two `Arc`s (a block map and a heights deque) and iterates
forward. The writer appends new blocks to its own map and pushes new hashes onto
the deque — both behind `ArcSwap`. No mutation to existing entries. No
`&mut self` on shared state. The theorem `chainstream_stable_across_writes`
proves that old and new snapshots are independent.

## 2. Data Safety

### Current: height-indexed lookups are reorg-unsafe

The non-finalized cache has two indexes:

```rust
// non_finalised_state.rs:86-94
pub(crate) struct NonfinalizedBlockCacheSnapshot {
    pub blocks: HashMap<BlockHash, IndexedBlock>,      // hash → block
    pub heights_to_hashes: HashMap<Height, BlockHash>,  // height → hash
    pub best_tip: BlockIndex,
}
```

`blocks` is safe: hash-keyed, stable during reorgs (a block's hash never changes).
`heights_to_hashes` is unsafe: during a reorg, the block at height 95 changes from
hash A to hash B. Any query that goes through `heights_to_hashes` can observe:

```
Thread 1: heights_to_hashes[95] → hash A
-- reorg occurs --
Thread 1: get_chainblock_by_hash(hash A) → not found (it was the old tip, now orphaned)
```

Or worse, within a single method:

```
get_indexed_block_by_height(95):
  hash = heights_to_hashes[95]           → hash A
  -- reorg: height 95 is now hash B --
  block = snapshot.get_chainblock_by_hash(hash A)  → None (height 95 now has hash B)
  // Falls through to finalized DB lookup by height 95
  // Returns hash B — but the caller asked for the block at the *original* height 95
```

### The snapshot "fixes" it by freezing time

Every query takes a `&Snapshot` parameter. `snapshot_nonfinalized_state()` captures
an `Arc<NonfinalizedBlockCacheSnapshot>` and queries read from that frozen copy.
This prevents intra-request races at the cost of forcing every method to carry a
`&Snapshot` parameter — even methods that don't touch non-finalized state (like
`get_treestate`, which always passes through to the validator).

### Block store: hash-keyed, immutable

In the block store, blocks are inserted by hash. The height→hash mapping is
append-only on the best chain. A block's hash never changes. A height→hash
lookup returns the same answer forever or returns nothing. No snapshot needed.
The formal model's `height_consistent` invariant (Section 2 of
[`BlockStore.lean`](./BlockStore.lean)) ensures every non-genesis block's parent
exists at `height - 1`, and this invariant is maintained by the ingestion path,
not by freezing mutable state.

## 3. Race Conditions

### Current: the snapshot prevents exactly one genuine race

Of the 9 `ChainIndex` methods that take a `&Snapshot`, here's what actually
needs it:

| Method | Takes `&Snapshot` | Genuine race without it? |
|---|---|---|
| `get_block_hash(height)` | Yes | **No** — single lookup, stale the moment it returns |
| `get_indexed_block_by_height(height)` | Yes | **No** — same |
| `get_block_range(start, end)` | Yes | **No** — client can't observe inconsistency across stream items |
| `get_compact_block(height)` | Yes | **No** — single lookup |
| `get_compact_block_stream(start, end)` | Yes | **No** — same |
| `get_transaction_status(txid)` | Yes | **No** — hash-based main path; best-chain check is advisory |
| `find_fork_point(hash)` | Yes | **No** — hash-based walk; best-chain check is advisory |
| `get_raw_transaction(txid)` | Yes | **No** — only peeks at tip for mempool branch ID |
| `get_tx_out_set_info` | No (creates its own) | **Yes** — accumulator walk across all heights would corrupt mid-reorg |

1 out of 9. And `get_tx_out_set_info` is not a gRPC method — it's an internal
JSON-RPC handler. The snapshot adds a `&Snapshot` parameter to 8 methods that
don't need it, purely to satisfy a consistency requirement that doesn't exist.

### The race that does exist: gRPC calls observe different chain states

Even with snapshots, a client making two sequential gRPC calls gets two
different snapshots. A reorg between calls means the two responses can
contradict each other. The snapshot doesn't help because it's intra-request
only, and each request creates a fresh one.

### Block store: races are impossible by construction

In an append-only hash-keyed store, there are no mutable entries to race on.
The only "mutation" is append. A reader either sees a block or doesn't.
The formal `insert_chain_from_old_unchanged` theorem proves that inserting a
new block never changes the result of a chain walk from an existing hash.

## 4. Data Consistency

### Current: consistency is deferred to the caller

The `ChainIndex` trait doesn't guarantee anything about ordering or
consistency across calls. Each method is individually consistent within
its snapshot, but:

- There's no guarantee that `get_block_hash(h)` and `get_block_height(hash)`
  are inverses — if a reorg lands between the two calls, they'll disagree.
- `best_chaintip` can return a tip that's already stale before the response is
  serialized.
- The mempool has its own chain tip (`mempool.mempool_chain_tip()`) that can
  diverge from the non-finalized snapshot's tip, producing `IncorrectChainTip`
  errors on `get_mempool_stream` — errors that are *expected* and handled, not
  bugs, but they surface the fundamental consistency gap.

### The two-tier split introduces an invisible seam

The finalized DB and non-finalized cache have an overlap boundary at
`NON_FINALIZED_DEPTH` (100 blocks). Queries that span this boundary must
merge results from two sources:

```rust
// get_block_range: for each height in [start, end]:
//   1. Try finalized DB (get_block_hash by height)
//   2. If not found, try snapshot (get_chainblock_by_height)
//   3. If not found, passthrough to validator
```

If a reorg shifts the boundary between steps, a block can be double-counted
or missed. The snapshot prevents *intra-request* boundary shifts, but the
seam itself is a design smell — it exists because the two stores have
different consistency models.

### Block store: single coherent model

The block store has one consistency model: append-only, hash-keyed. The
two-tier split (memory + LMDB) is an **implementation detail**, not a
semantic boundary. The formal model's `TwoTierInvariants` ensure:

- `mem_above_db`: blocks in memory are always at heights ≥ the DB tip
- `db_single_hash`: the DB has at most one hash per height
- `heights_dense`: the height deque covers every height from `heights_start`
  to tip

These invariants are maintained by the `freeze` operation (Section 6 of
[`BlockStore.lean`](./BlockStore.lean)), which atomically promotes blocks
from memory to disk. The reader's `ChainStream` sees a consistent view
regardless of where the boundary falls.

## 5. Performance

### Current: every gRPC call pays for a snapshot

```rust
// state.rs:1907 — GetBlock handler
let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
```

`snapshot_nonfinalized_state()` calls `get_snapshot()` which calls
`ArcSwap::load_full()` — an atomic refcount increment. Cheap per call (~10ns),
but every gRPC handler does it, and most don't need it. The cost is small
but unnecessary — 8 of 9 snapshot-consuming methods have no internal race
to protect against.

### The sync loop is a single-threaded bottleneck

```rust
// chain_index.rs:818
pub(super) fn start_sync_loop(&self) -> tokio::task::JoinHandle<...> {
    tokio::task::spawn(async move {
        loop {
            // ... get chain height, sync finalized, sync non-finalized ...
        }
    })
}
```

One task. One chain. If the validator is slow, every reader sees stale data.
If the sync loop panics, the entire index goes dark. There's no parallelism
in ingestion because the non-finalized cache is mutated in place and the
writer must be the sole mutator.

### The height-indexed compact block stream is expensive

`get_compact_block_stream` iterates heights one at a time, resolving each
through `snapshot.get_chainblock_by_height(height)` → hash → block. For a
range of 1000 blocks, that's 1000 height→hash lookups, 1000 block lookups,
and 1000 `compact_block_with_pool_types` transformations — all in a spawned
tokio task pushing through a bounded channel. A hash-keyed store resolves
the anchor once and walks forward by hash, one lookup per block, no height
resolution.

### Block store: O(1) cursor, lock-free reads

The `ChainStream` cursor (Section 5 of [`BlockStore.lean`](./BlockStore.lean))
is ~48 bytes: two `Arc` handles, four integers. Advancing is one hash lookup
and one deque index. No height resolution. No snapshot. The stream is
independent of the writer — the writer can publish new roots while the stream
is mid-flight, and the stream sees exactly what was committed when it started.

### Freeze is batched and commutative

The `freeze` operation promotes blocks from memory to LMDB in batches. The
formal model proves it preserves all invariants (`freeze_mem_above_db`,
`freeze_db_single_hash`). In practice this means the block store can freeze
in chunks (e.g. every N blocks) without blocking readers, and multiple freeze
operations compose.

## 6. Concrete Bugs

After thorough analysis, most of the snapshot-related complexity turns out to be
correct in normal operation: finalized blocks are immutable past the 99-block
reorg horizon, so stitching finalized and non-finalized data from different
sync-loop iterations produces a **stale but valid** chain — blocks link, hashes
match, no corruption. Staleness is inherent to any indexer and not a bug.

However, two real issues remain. One is a correctness bug (chain shortening).
The other is a set of unsafe code paths that are reachable in edge cases.

### Case A: Chain shortening is never detected

**`NonFinalizedState::sync`** at `non_finalised_state.rs:395-406`:

```rust
while let Some(block) = self
    .source
    .get_block(HashOrHeight::Height(
        u32::from(working_snapshot.best_tip.height) + 1,  // tip + 1
    ))
    .await?
{
    // handle normal append or reorg (via handle_reorg)
}
// If None → exit loop, publish snapshot as-is
```

The loop fetches blocks starting at `tip + 1`. It handles two cases:

- **Block returned, parent matches** → normal chain growth. Append to snapshot.
- **Block returned, parent doesn't match** → reorg at same tip height. `handle_reorg`
  walks backward to find the fork point and rebuilds the height map.

But there's a third case it **does not handle**:

- **No block returned** (`None`) → the validator's chain tip is below `tip + 1`.
  The loop exits silently. The snapshot keeps the old (now-orphaned) tip.

**Concrete scenario:**

```
1. Chain tip at height 500. NFS synced to 500.
2. A deep reorg shortens the chain to 490 (replaces blocks at 491-500 with nothing).
   Validator's best height is now 490.
3. Sync loop iteration:
   chain_height = source.get_best_block_height() = 490
   fs.sync_to_height(floor(490) = 391)  — finalized DB catches up
   nfs.sync(chain_height=490):
     while let Some(block) = source.get_block(Height(501)):
       → None (no block at that height)
       → loop exits
     → snapshot published with best_tip still at 500
4. Readers see best_chaintip = 500, blocks at 491-500 from the orphaned chain.
5. Next iteration: same thing. chain_height = 490, Height(501) = None, loop exits.
6. NFS permanently reports the wrong chain.
```

**Why it persists:** the while loop has no "did the chain shrink?" check. It only
walks forward from the current tip. A `handle_reorg`-style backward walk is
never triggered because no block is ever returned at `tip + 1`. The condition
`if u32::from(tip.height) + 1 > u32::from(chain_height)` at line 414 does cap
forward growth, but it only runs *inside* the loop body — unreachable when the
`while let` returns `None`.

**Practical impact:** deep chain-shortening reorgs are rare (require network
partitions or major consensus failures). But the code has no recovery path when
they occur — the indexer serves blocks from a chain that no longer exists.

### Case B: Height-indexed passthrough fallbacks

Several methods have a last-resort fallback that queries the live validator
**by height** when both local stores miss:

```rust
// compact_block_from_source, chain_index.rs:1010-1011
let Some(block) = source
    .get_block(HashOrHeight::Height(zebra_chain::block::Height(height.0)))
    .await?  // ← height-based query to live validator
```

This appears in:
- `compact_block_from_source` (used by `get_compact_block` and
  `get_compact_block_stream` as a fallback)
- `get_block_range` third fallback (`HashOrHeight::Height`)
- `best_chaintip` in `StillSyncingFinalizedState` variant

The validator returns whatever block is **currently** at that height. If the
snapshot was taken before a reorg and the fallback fires after, the returned
block is from a different chain than the snapshot's chain. The check
`if block_height != height` (line 1027) catches "wrong height" but cannot
catch "right height, wrong block."

**Reachability:** under normal operation these fallbacks should never fire —
the snapshot covers all non-finalized heights and the finalized DB covers the
rest. They exist as safety nets but are incorrect when they trigger.

### Why these are unfixable without architectural change

Both cases trace back to the same root cause: **height-indexed lookups that
assume the chain only grows forward.** The NFS sync loop fetches by height.
The fallbacks query by height. The design has no concept of "the chain at
this height might have changed or disappeared."

Fixing Case A would require adding a backward walk (similar to `handle_reorg`
but triggered by `None` instead of a parent mismatch), or detecting chain
shortening from the `chain_height` parameter and rebuilding the snapshot.
Both add complexity to an already fragile code path.

A hash-keyed block store avoids both issues by construction:
- Chain shortening? A `ChainStream` anchored to a hash walks forward from that
  hash. If the chain shortened, the stream simply ends where the chain ends.
- Height-indexed fallbacks? None exist. Every lookup is by hash. Height is
  metadata read off the block, not a lookup key.

## Summary

| Concern | Current ChainIndex | Block Store |
|---|---|---|
| **Concurrency** | Single writer, `ArcSwap` for readers. Correct but fragile — load/mutate/store pattern assumes exclusive writer | Append-only, no mutation. Writer and readers are independent |
| **Data safety** | `heights_to_hashes` is reorg-unsafe. Snapshot freezes it — a band-aid | Hash-keyed, immutable. Height is metadata, not a lookup key |
| **Race conditions** | 8 of 9 snapshot-taking methods don't need it. The one that does (`get_tx_out_set_info`) isn't gRPC-facing | No mutable state to race on. Races are impossible by construction |
| **Data consistency** | Two-tier split creates a seam at `NON_FINALIZED_DEPTH`. Mempool tip can diverge from chain tip | Single coherent model. Two-tier is an implementation detail, invisible to readers |
| **Performance** | Every gRPC call takes a snapshot. Sync loop is single-threaded. Height iteration is O(N²) in lookups | No snapshot overhead. Lock-free reads. O(1) cursor, forward-only walk |
| **Verification** | No formal model. Correctness argued by inspection | Formalized in Lean 4 with machine-checked proofs of thread safety, insertion, and freeze correctness |
