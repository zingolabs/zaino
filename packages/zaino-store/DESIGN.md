# Block Store Design

## Overview

The block store is a two-tier, thread-safe, snapshot-isolated store for Zcash
blocks. An in-memory persistent chain holds the last `MAX_REORG_DEPTH` blocks;
an on-disk LMDB freezer holds everything older. The sync loop is generic over a
`BlockFetcher` trait — it knows nothing about network protocols, block
serialisation, or validator-specific RPCs.

```
                  chain tip
                     │
  ┌──────────────────┼──────────────────┐
  │  Memory (Chain)                     │
  │  up to MAX_REORG_DEPTH = 101 blocks │
  │  im::Vector — structural sharing    │
  └──────────────────┼──────────────────┘
                     │ freeze_horizon = tip.height - MAX_REORG_DEPTH
  ┌──────────────────┼──────────────────┐
  │  LMDB                               │
  │  best-chain only, append-only       │
  │  height-keyed, hash + prev_hash +   │
  │  opaque payload per entry           │
  └─────────────────────────────────────┘
```

## State Model

### `Chain` — the in-memory chain

`Chain` wraps `Arc<im::Vector<Block>>` for O(1) clone and O(log n)
push/pop/truncate with full structural sharing. It is a dense height-indexed
sequence: `chain[i]` is the block at height `chain.start + i`. Every height
from `start` to `start + len - 1` has exactly one block.

```
pub(crate) struct Chain {
    pub(crate) start: Height,   // height of the first block (cs)
    inner: Arc<im::Vector<Block>>,
}
```

Key operations, all O(log n) and allocation-minimal:

| Method | What it does |
|---|---|
| `push_back(block)` | Append one block at the tip |
| `pop_front()` | Remove the head block (for freeze) |
| `truncate_from_incl(h)` | Keep `[start, h-1]`, drop `[h, …]` |
| `add_fragment(trim_from, frag)` | `truncate_from_incl(trim_from).append(frag)` |
| `get(height)` | O(log n) index lookup |
| `iter()` | `(Height, &Block)` iterator |

### `ChainState` — the shared root

`ChainState` holds the chain behind `RwLock<Arc<Chain>>` and an `Arc<LmdbStore>`.
LMDB is mandatory — all production code paths call `ChainState::open` which
creates the store at a filesystem path. `ChainState::new()` and
`with_start()` exist only for tests and will panic if an LMDB-requiring
operation is called.

Two derived quantities define the state:

| Variable | Definition |
|---|---|
| `cs` | `chain.start` — height of the first in-memory block |
| `ct` | `cs + chain.len()` — height of the next block to be added |

When the chain is empty, `cs == ct`. Neither is stored — both are computed from
the `Chain`.

## Core Types

```rust
pub type BlockHash = [u8; 32];
pub type Height = u32;

pub struct Block {
    pub hash: BlockHash,
    pub height: Height,
    pub prev_hash: BlockHash,
    pub data: Vec<u8>,       // opaque payload — the store never interprets it
}
```

`MAX_REORG_DEPTH = 101`. The Zcash protocol guarantees reorgs ≤ 100 blocks.
The +1 accounts for the backward walk needing to reach `h = fork + 1` to test
`h - 1 = fork` — testing the fork point itself costs an extra step.

## LMDB Freezer

`LmdbStore` is an append-only height-keyed database. Each height maps to a
value of `[hash: 32 bytes][prev_hash: 32 bytes][payload: …]`. A sentinel key
`[0xFF; 4]` stores the block count (= `cs`) for crash recovery.

| Detail | Value |
|---|---|
| Key format | 4-byte big-endian height |
| Value format | 32-byte hash + 32-byte prev_hash + opaque data |
| Map size | 512 GB |
| DB name | `"blocks"` |
| Sentinel | `[0xFF; 4]` → 4-byte big-endian block count |

`LmdbStore::truncate_to_height(max_height)` deletes all entries above
`max_height` and updates the sentinel. This is an admin tool for trimming a
corrupted chain back to a known-good height before restarting sync — it is not
used in normal operation.

## Concurrency Model

```
Writer (sync loop)                    Reader (stream_blocks / get_block)
──────────────                        ─────────────────────────────────
new_chain = chain.push_back(b)  ← lock-free (structural sharing)

─── write lock ───                    ─── read lock ───
state.chain ← Arc::new(new_chain)     snap = state.chain.clone()  ← Arc bump
─── unlock ───                        ─── unlock ───

                                      for h := start to end:   ← lock-free
                                          resolve + yield
```

- **Writer critical section:** one `Arc` pointer assignment (~nanoseconds).
- **Reader critical section:** one `Arc::clone` call (~nanoseconds).
- **All real work** (Chain construction, block resolution, iteration) happens
  outside any lock.

## Operations

### `ingest(hash, block)` — single-block tip extension

Validates `prev_hash` and `height` continuity against the current tip, then
pushes the block. Returns `Err` on mismatch.

### `ingest_batch(blocks)` — multi-block tip extension

Validates internal chain continuity across the batch (each block's `prev_hash`
matches the previous block's hash) and that the first block extends the current
tip. Appends atomically. For reorgs, use `add_fragment` instead.

### `add_fragment(trim_from, fragment)` — reorg / tip-extension

Truncates the chain at `trim_from` (inclusive), then appends `fragment`. Keeps
`[cs, trim_from - 1]`, replaces `[trim_from, ct)`. This is the general-purpose
mutation: a normal tip extension is `add_fragment(ct, [new_block])`; a shallow
reorg is `add_fragment(fork + 1, fragment)`. Accepts `im::Vector<Block>` and
swaps the chain root under the write lock.

### `flush_chain_to_lmdb()` — persist everything

Writes all in-memory chain blocks to LMDB in one transaction, then resets the
chain to empty at the new `cs`. Post-condition: `cs == ct`, chain is empty.
Panics if LMDB is not configured (must be called only on a `ChainState`
opened via `open`).

### `append_to_freezer(blocks)` — forward-fill write

Writes blocks directly to LMDB without them ever entering the in-memory chain.
Pre-condition: chain must be empty (caller must have flushed first). Advances
`cs` and `ct` by `blocks.len()`. Panics if LMDB is not configured.

### `trim_chain()` — enforce depth bound

If `ct - cs > MAX_REORG_DEPTH`, freezes the excess blocks from the head of the
chain to LMDB via repeated `pop_front`. Post-condition: `ct - cs ≤ D`.
Panics if LMDB is not configured.

### `freeze()` — horizon-based archival

Freezes chain blocks whose height is strictly below `tip.height -
MAX_REORG_DEPTH`. Unlike `trim_chain` which freezes a fixed count, `freeze`
freezes everything below the horizon regardless of chain length. Both LMDB
writes and chain pops happen in batch.

## Sync Algorithm

`sync_step` is one iteration. It is generic over `BlockFetcher`, which
provides three methods:

```rust
#[async_trait]
pub trait BlockFetcher {
    type Error: std::fmt::Display + Send + 'static;
    async fn fetch_tip(&self) -> Result<(BlockHash, Height), Self::Error>;
    async fn fetch_batch(&mut self, from: Height, to: Height) -> Result<Vec<(BlockHash, Block)>, Self::Error>;
    async fn fetch_at_height(&mut self, height: Height) -> Result<Block, Self::Error>;
}
```

```
sync_step(state, fetcher):

1.  Fetch remote tip: (remote_hash, rt) = fetcher.fetch_tip()

2.  Early exit: if remote_hash == state.tip():
        trim_chain()
        return Ok(())           // nothing changed — one RPC call, no mutation

3.  Compute gap: gap = rt - ct  (signed; negative → remote behind us)

4.  Forward fill (gap ≥ D):
    a. flush_chain_to_lmdb()    // persist everything in memory
    b. to = rt - D
    c. While ct ≤ to:
         batch = fetcher.fetch_batch(ct, min(to, ct + MAX_BATCH_SIZE - 1))
         append_to_freezer(batch)

5.  Slow sync (always, unless early-exit fired):
    (trim_from, fragment) = find_trim_index(state, fetcher, rt, fuel=D)
    append_to_chain(trim_from, fragment)

6.  trim_chain()

7.  Assert: ct - cs == chain.len()
    Assert: ct - cs ≤ D  (when LMDB is present)
```

`MAX_BATCH_SIZE = 1000` caps the number of blocks fetched in a single forward-
fill RPC, preventing the initial sync from fetching millions of blocks in one
blocking call.

### Early exit

When `remote_hash == state.tip()`, the store is already synced. The algorithm
returns immediately after `trim_chain()` — one RPC call, zero state mutation.
Without this check, every iteration would run the slow-sync backward walk (one
RPC per step) and log "applied fragment of length 0".

### Forward fill

When `gap ≥ D`, the remote is far enough ahead that the backward walk (fuel =
D) cannot reach our chain. The algorithm flushes any in-memory blocks to the
freezer, then bulk-fetches `[ct, rt - D]` in batches of up to
`MAX_BATCH_SIZE` and writes them directly to LMDB via `append_to_freezer`.
After forward fill, `cs = ct = rt - D + 1`, and the slow sync handles the
final D blocks `[rt - D + 1, rt]` into the chain.

### Slow sync: `find_trim_index`

Walks backward from the remote tip one block at a time, accumulating a fragment
and searching for the fork point — the first height where the remote block's
`prev_hash` matches a local block. Returns `(trim_from, fragment)` where
`trim_from` is the first height to replace (inclusive). The caller keeps the
local chain up to `trim_from - 1` and appends `fragment`.

The implementation mirrors the Lean `findTrimIndex` formalisation in
`docs/lean/Proof.lean`. The outer function `find_trim_index` initialises the
accumulator and delegates to `find_trim_index_int` — an unrolled loop
translation of the tail-recursive Lean `findTrimIndexInt`.

```
find_trim_index_int(state, fetcher, h, fragment, fuel, depth):

    loop:
        if fuel == 0 → Err(ReorgTooDeep { depth })
        fuel -= 1

        block = fetcher.fetch_at_height(h)
        fragment.push_front(block)

        if h == cs:
            if cs == 0:
                assert block.prev_hash == GENESIS_HASH  // genesis check
            else:
                expected = state.get_block_by_height(cs - 1)
                assert block.prev_hash == expected.hash  // boundary link
            return (cs, fragment)

        // h > cs: check chain[h-1]
        if chain.get(h - 1) matches Some(cb) && block.prev_hash == cb.hash:
            return (h, fragment)

        h -= 1   // keep walking down
```

Three termination cases:

1. **Fork in chain** (`h > cs`, `chain[h-1].hash == block.prev_hash`): return
   `(h, fragment)`. The local block at `h-1` is the common ancestor. The chain
   is truncated from `h` onward and the fragment is appended starting at `h`.

2. **Fork at freezer boundary** (`h == cs > 0`, `freezer[cs-1].hash ==
   block.prev_hash`): return `(cs, fragment)`. The common ancestor is the last
   block in LMDB. The entire chain is replaced.

3. **Fuel exhausted** (`fuel == 0`): return `Err(ReorgTooDeep)`. The fork
   point is deeper than `MAX_REORG_DEPTH` — the reorg cannot be resolved by
   the sync loop and requires a full resync.

The caller then validates internal fragment contiguity
(`fragment[i].hash == fragment[i+1].prev_hash`) and applies the fragment via
`append_to_chain`.

### Reorg handling

The backward walk (`find_trim_index`) always runs unless the early-exit
catches the already-synced case. When the remote tip's `prev_hash` chain
diverges from the local chain at some height, the walk finds the fork
point regardless of whether the remote is ahead or behind. The chain is
truncated at the anchor and rebuilt with the remote's fragment.
`trim_chain` then freezes any excess from the head.

## Reading

### Point lookups

- **`get_block_by_hash(hash)`**: scans the chain (O(n), bounded to
  `MAX_REORG_DEPTH`). The chain is height-indexed, not hash-indexed.
- **`get_block_by_height(height)`**: tries the in-memory chain first; if the
  height is below `chain_start`, falls through to LMDB.

### `ChainStream` — snapshot cursor

A reader captures an `Arc<Chain>` under the read lock (one pointer bump) and
creates a `ChainStream` cursor: `{ chain, freeze_horizon, current, end }`.
Iteration is a forward for-loop — no backward walk, no accumulation buffer.
O(1) memory regardless of range size. The cursor is independent of concurrent
writes; it sees exactly the chain that was committed when the snapshot was
taken.

**Implementation note:** capturing the `Arc<Chain>` pins the
`Arc<im::Vector<Block>>` inside it. `im::Vector` uses a persistent RRB tree
with a 64-ary branching factor — for a 101-element chain the spine depth is
just 2 (one internal node, two leaf chunks). Concurrent snapshots share
internal nodes via structural sharing: the writer's new versions reuse old
tree nodes, only allocating new spine nodes along the path from root to the
changed leaf.

The practical concern is slow clients: each reader that holds a snapshot
pins the spine path of that specific version. 1000 concurrent readers across
1000 distinct writer versions would retain 1000 spine paths, each path ~2
tree nodes containing 64 child `Arc`s. The retained tree nodes are shared
across versions for the overlapping suffix of the chain, so the marginal
cost per reader is small (~2 copies of a 64-slot node header). Still, the
number of retained versions is technically bounded only by the number of
concurrent in-flight snapshots. Connection eviction (closing streams whose
cursor makes no progress for a timeout) limits the window to active clients
and is the appropriate defence-in-depth measure.

### `BlockIter` — unified two-tier iterator

`ChainState::stream_blocks(start, end)` returns a `BlockIter` that
transparently handles the LMDB/in-memory boundary. Heights below `chain_start`
are served from LMDB; heights at or above `chain_start` are served from a
`ChainStream` snapshot taken at creation time. Callers don't need to know
about the two-tier layout.

## Recovery

On restart, `ChainState::open(path, start_height)` reads
`cs = freezer.block_count()` from the LMDB sentinel. The chain starts empty
(`ct = cs`, `chain = []`). The first `sync_step` will forward-fill the bulk
gap (if any) from `cs` up to `rt - D`, then slow-sync the top D blocks into
the chain. No additional recovery logic is needed.

If the LMDB is empty, `cs` is set to the caller-supplied `start_height` (e.g.
Sapling activation height).

## Sync Loop Runner

`BlockStoreSync<F>` wraps a `ChainState`, a `BlockFetcher`, and `SyncTimings`,
running `sync_step` on a loop:

```rust
pub struct SyncTimings {
    pub interval: Duration,              // 500ms
    pub initial_backoff: Duration,       // 250ms
    pub max_backoff: Duration,           // 8s
    pub max_consecutive_failures: u32,   // 10
}
```

- **Success**: sleeps for `interval`, then runs again.
- **Failure**: exponential backoff from `initial_backoff` to `max_backoff`.
  After `max_consecutive_failures` consecutive failures, the loop exits with a
  warning.
- **Cancellation**: a `CancellationToken` allows graceful shutdown.

The loop logs initial sync completion with elapsed time, start/end heights, and
throughput (blocks/s during forward fill).

## Error Types

```rust
pub enum StoreError {
    HeightNotFound(Height),          // height not in the chain or LMDB
    BelowFreezeHorizon(Height, Height), // height is in LMDB, not chain
    InsertionFailed(String),         // prev_hash or height mismatch
    FreezeError(String),             // LMDB I/O error
    InvariantViolation(String),      // internal invariant broken
}

pub enum SyncError {
    Fetch(String),                   // fetcher returned an error
    Store(StoreError),               // store operation failed
    ChainIncoherent { height, expected, got },  // fragment link broken
    ReorgTooDeep { depth },          // fork not found within MAX_REORG_DEPTH
}
```

`ChainIncoherent` is returned when the backward walk fetches a block whose hash
doesn't match the expected `prev_hash` from the previous step — the remote
chain changed during the walk (a reorg in flight). The caller should discard
and retry; no local state has been mutated.

`ReorgTooDeep` means the fork point is beyond `MAX_REORG_DEPTH` blocks. The
sync loop cannot resolve this and exits; the operator must resync from scratch
or from a known-good checkpoint.

## Why D = 101

The Zcash protocol guarantees reorgs ≤ 100 blocks. The backward walk
(`find_trim_index`) must reach `h = fork + 1` to test `h - 1 = fork` — the
fork point itself costs an extra step. So `D = N + 1 = 101`.

This bounds the in-memory chain to at most 101 blocks under normal operation
(after trimming). After `append_to_chain`, the chain may temporarily hold up to
`2·D = 202` blocks before `trim_chain` brings it back to ≤ 101.

## Crate Structure

```
packages/zaino-store/src/
  lib.rs           — crate root, re-exports public API
  types.rs         — Block, BlockHash, Height, MAX_REORG_DEPTH, genesis
  chain.rs         — Chain (im::Vector-backed persistent sequence)
  state.rs         — ChainState (RwLock<Arc<Chain>> + optional LmdbStore)
  lmdb.rs          — LmdbStore (height-keyed, sentinel-based recovery)
  chain_stream.rs  — ChainStream (snapshot cursor)
  block_iter.rs    — BlockIter (unified LMDB + ChainStream iterator)
  fetcher.rs       — BlockFetcher trait
  sync.rs          — sync_step, find_trim_index, BlockStoreSync, SyncTimings
  error.rs         — StoreError, SyncError
```

## Lean Formal Proofs (`docs/lean/Proof.lean`)

A mechanised specification and correctness proof of the sync algorithm in
Lean 4. It models every pure operation that `sync_step` calls (`flush`,
`appendFreezer`, `addFragment`, `trim`, `findTrimIndex`), their composition
into `sync_step`, and the `List.IsChain` preservation across the state
transitions. IO is abstracted, blocks are generic, and the control flow
(early exit loop, forward fill loop, batching) is left to the Rust
implementation — the focus is on size invariants after each operation and
chain contiguity.

### Numeric state model

A `State` holds three natural numbers with the invariant `ct = cs + cl`:

```
cs   — freezer height (number of blocks in LMDB)
ct   — height of the next block to be added
cl   — chain length (= ct - cs)
```

Every operation (`flush`, `appendFreezer`, `appendChain`, `trim`,
`trimToAnchor`, `addFragment`) is a pure function on `State` plus a
length `n` or `flen` for the appended fragment. Each comes with
post-condition theorems, e.g.:

| Theorem | What it says |
|---|---|
| `flush_cl_zero` | After flush, `cl = 0` |
| `trim_cl_bounded` | After trim, `cl ≤ D` |
| `appendChain_cl_bounded` | If `cl ≤ D` then after append `cl ≤ 2·D` |
| `addFragment_cs_unchanged` | `addFragment` never moves `cs` |
| `sync_step_bounded` | After `trim`, `cl ≤ D` |

### Chain contiguity (`List.IsChain`)

The theorems operate on an abstract linking relation `R : α → α → Prop`.
Each operation is shown to preserve `List.IsChain R (freezer ++ chain)` —
the concatenation of LMDB and memory blocks stays linked. The key results:

| Theorem | What it says |
|---|---|
| `trim_full_chain` | Trim preserves the contiguous prefix |
| `appendFreezer_full_chain` | Appending to an empty chain preserves contiguity |
| `appendChain_full_chain` | Extending the chain preserves contiguity (given a linking condition at the boundary) |
| `addFragment_full_chain` | Truncating at `trim_from` and appending a fragment yields a contiguous list |

### Realization bridge

A `Realization` bundles concrete lists (freezer and chain) with length
proofs matching the numeric `State` and a `List.IsChain` proof for their
concatenation. Each operation can be lifted to a `Realization`:

```
Realization(s)  ──flush──→  Realization(flush(s))
Realization(s)  ──appendChain──→  Realization(appendChain(s, flen))
Realization(s)  ──addFragment──→  Realization(addFragment(s, trim_from, flen))
```

This means any property proved on the numeric `State` automatically
holds for any concrete list realisation — the proofs transfer.

### `findTrimIndex` — the backward walk

A tail-recursive function (`findTrimIndexInt`) walks from the remote tip
downward, fetching blocks and accumulating a fragment. It terminates
structurally on `fuel` (which starts at `D = 101`). Three outcomes:

| Outcome | Meaning |
|---|---|
| `.ok(cs, acc')` | Fork at the freezer boundary — the last LMDB block is the common ancestor |
| `.ok(h, acc')` with `h > cs` | Fork in the chain — the local block at `h-1` is the common ancestor |
| `.error .fuelExhausted` | Fuel depleted before finding the fork |
| `.error .chainIncoherent` | Genesis `prev_hash` mismatch |

A bridge theorem (`findTrimIndex_realization`) composes a successful
`findTrimIndex` result with a `Realization` and a well-formed fragment
into a post-`addFragment` `Realization`, proving that a successful
backward walk plus `addFragment` preserves the `List.IsChain` invariant.

The theorem `findTrimIndex_cs_le_trim_from` proves that the returned
`trim_from` is never below `cs` — the walk never claims the common
ancestor is in the freezer when it's not.
## Test Coverage

Tests live alongside the code in `#[cfg(test)] mod tests` blocks. The
`MockFetcher` — an in-memory `HashMap<Height, (BlockHash, Block)>` implementing
`BlockFetcher` — drives the sync tests without a network.

### Chain (structural sharing) — `chain.rs`

| Test | What it verifies |
|---|---|
| `chain_push_and_get` | Push two blocks; old chain snapshot unchanged after push |
| `chain_pop_front` | Pop advances `start`; old snapshot still has the popped block |
| `chain_truncate_from_incl` | Truncate at midpoint; original chain untouched |
| `chain_add_fragment_tip_extend` | `add_fragment` at `start + len` appends cleanly |
| `chain_add_fragment_reorg` | `add_fragment` at height 2 replaces fork; old chain unchanged |
| `chain_structural_sharing` | Clone before push; snapshot sees only the original blocks |

### ChainState (ingestion, freeze, snapshot isolation) — `state.rs`

| Test | What it verifies |
|---|---|
| `ingest_extends_chain` | Single-block ingestion extends tip; lookup by hash and height works |
| `ingest_rejects_duplicate_hash` | Wrong `prev_hash` → `InsertionFailed` |
| `stream_range_snapshot_isolation` | Stream taken at height 1 stays at height 1 after ingesting height 2 |
| `ingest_batch_valid_extends_tip` | Three-block batch from genesis; tip and height lookups correct |
| `ingest_batch_rejects_bad_tip_extension` | Batch with wrong `prev_hash` → error mentioning "ingest_batch" |
| `ingest_batch_rejects_internal_break` | Batch where block[1] doesn't link to block[0] → "internal chain break" |
| `ingest_batch_empty_succeeds` | Empty batch on genesis store is a no-op; tip stays genesis hash |
| `ingest_batch_empty_store_accepts_valid_chain` | Batch starting at height 100 (non-zero `chain_start`) |
| `add_fragment_tip_extension` | `add_fragment` at `ct` extends tip cleanly |
| `add_fragment_reorg` | Fork at height 2 replaces blocks 2..3 with alternative chain |
| `freeze_moves_below_horizon_blocks_to_lmdb` | Ingest 112 blocks (genesis + 111), freeze → 10 blocks in LMDB, chain at `cs=10`, `get_block_by_height` serves frozen heights from LMDB |
| `reopen_restores_frozen_blocks_from_lmdb` | Session 1 freezes 10 blocks; Session 2 reopens — `chain_start=10`, frozen heights served from LMDB, unfrozen heights gone |
| `reorg_after_freeze_frozen_blocks_still_served` | Freeze 10 blocks, then reorg at height 16 — frozen blocks (0..9) still served from LMDB; old fork blocks above 16 gone; new fork blocks present |
| `stream_snapshots_diverge_after_reorg` | Pre-reorg stream sees old chain (all tag 0); post-reorg stream sees mixed chain (heights 0..2 tag 0, 3..5 tag 1); live state has new blocks only |

### Sync algorithm — `sync.rs`

| Test | Scenario | What it verifies |
|---|---|---|
| `trim_found_at_local_tip_normal_extension` | Local 0..10, remote 11..20 same fork | `trim_from=11`, fragment has 10 blocks |
| `trim_found_after_shallow_reorg` | Local 0..10 tag 0, remote diverges at 6 tag 1 | `trim_from=6`, fragment has 7 blocks (6..12) |
| `trim_not_found_when_fuel_exhausted` | Local 0..10 tag 0, remote 0..20 tag 1, fuel=3 | `ReorgTooDeep { depth: 3 }` — state untouched |
| `sync_step_normal_extension` | Local 0..5, remote 6..10 same fork, gap=5 ≤ D | End-to-end: tip advances to 10, all heights 1..10 served |
| `sync_step_reorg_truncate_and_rebuild` | Local A(1..5), remote fork B(4..7) | Tip = B7, heights 1..3 survive, 4..5 gone, 4..7 present |
| `sync_step_deep_divergence_fuel_exhausts_state_untouched` | Local tag-0 at cs=6, remote tag-1 disjoint, fuel=101 | `ReorgTooDeep`, tip and all 101 local blocks untouched |
| `sync_step_forward_fill_large_gap` | LMDB holds 0..999, chain empty at cs=1000, remote tip=1250 | Forward fill writes 1000..1149 to LMDB, slow sync populates 1150..1250 in chain, `ct-cs=101` |
| `sync_step_negative_gap_reorg` | Local cs=10, blocks 10..110; remote diverges at 51, tip=80 | gap=-31 → reorg: chain truncated at 50, fork-B blocks 51..80 appended; old 81..110 gone |
| `sync_step_already_synced_noop` | Local and remote both at 10, same hash | Early exit after one RPC; tip and tip_height unchanged |
| `sync_step_chain_exceeds_d_then_trimmed` | Local has genesis + 101 blocks (102 > D), remote extends 30 more | After sync: tip=131, cs advanced to 31, LMDB has 31 frozen blocks, `ct-cs=101` |

### LMDB — `lmdb.rs`

| Test | What it verifies |
|---|---|
| `truncate_to_height_removes_blocks_above_max` | Write 0..=5 (6 blocks), truncate to 2 → 3 deleted, count=3, 0..=2 still present, 3..=5 gone; truncate above latest is no-op |

### ChainStream cursor — `chain_stream.rs`

| Test | What it verifies |
|---|---|
| `chain_stream_iterates_forward` | Stream over heights 0..2 yields blocks in order, then `None` |
| `chain_stream_below_freeze_returns_error` | `freeze_horizon=1`, start=0 → first `next()` returns `Err(BelowFreezeHorizon)` |
| `chain_stream_start_above_zero` | Chain starting at height 5 → stream over 5..6 yields correct blocks |

### BlockIter — `block_iter.rs`

| Test | What it verifies |
|---|---|
| `stream_blocks_in_memory_only` | `stream_blocks(0, 1)` yields genesis then block 1, then `None` |
| `stream_blocks_empty_range` | `stream_blocks(5, 4)` returns `None` immediately |

### Patterns exercised

- **Snapshot isolation:** `stream_range_snapshot_isolation`, `chain_structural_sharing`,
  `stream_snapshots_diverge_after_reorg` — old `Arc<Chain>` snapshots survive
  concurrent writes
- **Reorg safety:** `add_fragment_reorg`, `sync_step_reorg_truncate_and_rebuild`,
  `sync_step_negative_gap_reorg`, `reorg_after_freeze_frozen_blocks_still_served` —
  fork detection, truncation, and rebuild preserve frozen blocks
- **Crash recovery:** `reopen_restores_frozen_blocks_from_lmdb` — LMDB sentinel
  restores `cs`; frozen blocks served from disk; unfrozen chain blocks are lost
  (re-synced on next loop iteration)
- **Deep reorg rejection:** `trim_not_found_when_fuel_exhausted`,
  `sync_step_deep_divergence_fuel_exhausts_state_untouched` — local state is
  untouched on `ReorgTooDeep`
- **Forward fill:** `sync_step_forward_fill_large_gap` — bulk LMDB write path
  with gap ≥ D
```
