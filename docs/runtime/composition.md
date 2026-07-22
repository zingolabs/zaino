# Runtime Composition — Block-Store Spine ⊕ Typed-Index Layer

**Status:** design direction. Open questions below are unresolved — this is not
yet an ADR, it's the shape we're converging on and the decisions still owed.

## Problem

The runtime (subdomain **C** — the orchestrator that implements the inner
driving surface `zaino-service::IndexerService`) must own four things the
current code splits badly:

- a reorg-safe view of the recent chain (**NFS**),
- an append-only finalized store (**FS**),
- a single coherent **feed** that advances both,
- a **snapshot** that serves coherent *cross-request* reads across both.

Three existing efforts each cover part of this; none covers all of it:

- **`zaino-sync`** (new engine) builds *typed indexes* over finalized blocks
  (batch/DAG). **FS only** — explicitly does not model NFS or reorg.
- **Legacy `NodeBackedChainIndex`** (`zaino-state`) models NFS+FS+feed+snapshot,
  but unsoundly: three independent validator pollers, a fragile single-writer
  `load → mutate → store`, reorg-unsafe `height→hash` lookups, a two-tier seam,
  and an undetected chain-shortening reorg bug (rationale §6 Case A; confirmed by
  our own behavioural survey).
- **Hahn's `zaino-store`** (PR #1378) is a two-tier reorg-safe **block** store
  with a **Lean-verified** sync/reorg algorithm — but it is block-only (no typed
  indexes / integrity model), best-chain-only, and argues snapshots are "mostly
  unnecessary" (disputed by review).

## Decision (direction)

Build the runtime as two composed layers, split by **layer** (block-spine vs
typed-index), **not** by **tier** (FS vs NFS). Both layers span both tiers, in
their own currency.

```
                          incoming reads (zaino-service::IndexerService)
                                          │
   ┌──────────────────────────────────────┼───────────────────────────────┐
   │  Snapshot = { pinned Arc<Chain>, finalized_watermark }                 │
   │     above watermark → pinned Chain (NFS, reorg-bound, actively pinned) │
   │     below watermark → finalized store (append-only, passively coherent)│
   └──────────────────────────────────────┼───────────────────────────────┘
                                           │
   Layer 1 — BLOCK SPINE (zaino-store)     │   Layer 2 — TYPED INDEXES (zaino-sync)
   ───────────────────────────────────    │   ──────────────────────────────────
   Chain    : in-mem ~101 blocks (NFS)     │   headers / address / spends / txid-loc
   Freezer  : on-disk blocks     (FS)      │   built ONLY over the spine's FROZEN
   sync_step: one feed / BlockFetcher      │   (finalized, immutable) blocks.
   reorg    : find_trim_index (Lean)       │   NFS-window typed queries RE-DERIVE
   serves   : block / compact by height    │   on demand from the in-mem Chain.
   hands out: the Snapshot (pins Chain)    │   → no NFS/reorg machinery needed.
                                           │
        blocks flow ─────────── freeze horizon ──────────▶ index over frozen blocks
```

### Layer 1 — Block spine (`zaino-store`-shaped)

- `Chain` (in-memory, ~101 blocks, persistent vector, structural sharing) = NFS
  blocks; `Freezer` (on-disk) = FS blocks. **Payload per height is a whole
  `CompactBlock`** (Q4) — the canonical serving unit, `ChainMetadata` included;
  the recent window additionally carries reorg metadata (chainwork), i.e. an
  `IndexedBlock`-equivalent. `freeze_horizon = tip − MAX_REORG_DEPTH` is the one
  boundary.
- A companion **side-branch set** (`Arc<HashMap<BlockHash, Block>>`) retains
  recent non-best blocks for fork-serving queries (Q2, decided). Pinned by the
  Snapshot alongside the best-chain `Chain`.
- **One** `sync_step`-style feed over a `BlockFetcher` port: early-exit /
  forward-fill / slow-sync (backward-walk fork-find). **Reorg is handled here,
  once, and is Lean-verified.**
- Serves block/compact-by-height queries and hands out the **Snapshot** (pins an
  `Arc<Chain>`).

### Layer 2 — Typed index layer (`zaino-sync` + `zaino-indexes`)

- **Auxiliary reverse-lookup indexes only** — `hash_to_height`, `txid_location`,
  `transparent_spends`, future address-history, and a lean `headers` for
  fork-walking — built over the spine's frozen (finalized, immutable) blocks.
  These are exactly the indexes queried *individually* that a block store can't
  serve (Q4). The compact-block **body** is *not* decomposed — it's stored whole
  in the spine.
- NFS-window auxiliary queries **re-derive on demand** from the spine's in-memory
  `Chain` (~101 blocks — cheap).
- Because it only ever indexes immutable blocks, it needs **no NFS/reorg
  machinery** — the exact thing that makes a purely index-centric design hard.

### The Snapshot (spans both)

`Snapshot = { pinned Arc<Chain>, pinned Arc<side-branches>, finalized_watermark }`.
Reads above the watermark hit the pinned `Chain` (reorg-bound, actively pinned);
fork-serving queries (`getchaintips`, orphaned-fork tx-status, fork-point-by-hash)
hit the pinned side-branch set; reads below the watermark hit the finalized store
(append-only/immutable, passively coherent). This is the **FS-passive /
NFS-pinned** asymmetry we derived independently, and it matches ADR-0003's
cross-request pinning requirement.

## Why this beats either alone

- **Reorg lives in one proven place** (the spine), not smeared across N indexes.
- **Typed indexes only ever see immutable blocks** → simple, batch, no rollback.
- The **dominant lightwallet workload** (compact-block-by-height streaming) is
  served directly by the spine, reorg-safe, with an O(1) cursor.
- **Snapshots are cheap** — an `Arc` pin over a persistent vector.

## Mapping onto the new hexagonal arch

Hahn's store lives on `dev` with its own types; adapting it *inward*:

| Hahn's `zaino-store` | New-arch home |
|---|---|
| `BlockFetcher` trait | a `zaino-source`-shaped **driven port** |
| `Block { data: Vec<u8> }` (opaque) | payload stays opaque bytes at the spine; typed decode happens in the index layer. Ids reconcile with `zaino-primitives` |
| its own LMDB | `zaino-persistence` / a block backend (may stay **distinct** from the index backend) |
| `sync_step` loop | one of the runtime's **producer tasks** |
| `ChainState` (`RwLock<Arc<Chain>>`) | the **runtime-owned** NFS cell |
| — | the runtime **implements** `zaino-service::IndexerService` |

## Design decisions & open questions

Pressure-tested against `zaino-store`'s code (`chain.rs`, `chain_stream.rs`,
`state.rs`), the legacy serving code, and `sync.rs`: **Q1, Q2, Q4 and Q5 are
resolved** — only Q3 (integrity/versioning) remains. His `im::Vector` `Chain`,
Lean-verified `sync_step`/`find_trim_index`, and lock-minimal concurrency are all
keepers.

### Resolved

- **Q1 — First-class snapshot → ADD a `snapshot()` handle.** Structurally
  supported: `ChainState` is `RwLock<Arc<Chain>>`, `Chain` is an immutable
  `Arc<im::Vector>` (O(1) clone). Hahn exposes only per-call reads and
  range-scoped `ChainStream`/`BlockIter`, never a captured handle — so no stable
  view across related requests (the reviewer concern; `stream_snapshots_diverge_
  after_reorg` proves the pin itself works). Add `fn snapshot(&self) -> Snapshot`
  = one `chain.read().clone()` + LMDB handle + freeze-horizon, serving arbitrary
  queries from the pinned `Chain` (≥ its start), falling through to append-only
  LMDB below. Immutable finalized blocks make pinned-`Chain` + live-`Freezer`
  coherent across the boundary. Adopts his structure, rejects his "snapshots
  unnecessary" framing (ADR-0003).
- **Q2 — Side branches → RETAIN them (decided).** We commit to serving
  `getchaintips`, orphaned-fork transaction-status, and fork-point-by-hash from
  memory, so non-best blocks must be kept. `Chain` is strictly best-chain (dense
  vector), so add a **companion `Arc<HashMap<BlockHash, Block>>`** of recent
  non-best blocks alongside it, pinned by the same `Snapshot`. Additive, moderate.
  Note: reorg *resolution* doesn't need this (`find_trim_index` re-fetches the
  fork from the source) — only *serving* fork-queries does.
- **Q4 — Finalized store → whole `CompactBlock`s + auxiliary reverse indexes
  only (decided; verified against serving code).** No client endpoint reads a
  body index (`txids`/`sapling`/`orchard`/`transparent`) at height to serve —
  those height-keyed reads occur *only* in reassembly (the ephemeral backend
  rebuilding compact blocks) and the internal `get_tx_out_set_info` accumulator;
  single-tx serving reads the body by tx-location, i.e. an index into the
  height's list ("read the block, pick `tx_index`"). The reverse indices
  (`hash_to_height`, `txid_location`, `transparent_spends`/spender) *are* called
  individually throughout serving and a block store can't replace them. So the
  spine stores **whole `CompactBlock`s** (`ChainMetadata` rides along — no
  separate tree-size index; the stubbed body decoders become moot) and the index
  layer builds **only the individually-queried auxiliary indexes**. The 8-way
  body decomposition is dropped. **Full `Block` / raw-tx / proofs** (what compact
  discards) are served by **validator passthrough, not stored** — the legacy
  pattern (`get_fullblock_bytes_from_node`, `get_raw_transaction` → node; the new
  arch stores no full blocks either). A full-block cache is a future optimization
  *only if* whole-chain full-block streaming (zallet scan) becomes a hot path.
  *Minor:* `get_tx_out_set_info` walks `transparent` over
  all heights — reads whole blocks under this model, or keeps a lean transparent
  index (low-frequency internal JSON-RPC).
- **Q5 — Adopt vs re-implement → ADOPT the algorithm, re-skin 3 seams (decided;
  verified in `sync.rs`).** `sync_step`/`find_trim_index`/`BlockStoreSync` are
  generic over `BlockFetcher` + `ChainState` and never touch LMDB or
  serialization directly (that lives in `state.rs`/`lmdb.rs`), so the
  Lean-mirrored algorithm ports near-verbatim. Re-skin: `BlockFetcher` → a
  `zaino-source`-shaped port; `types.rs` `Block`/`Height`/`BlockHash` →
  `zaino-primitives` (payload stays opaque = serialized `CompactBlock`, per Q4);
  `state.rs`/`lmdb.rs` → keep his LMDB or map to `zaino-persistence`.
  **Placement: a new `zaino-nfs` crate, *not* a `zaino-core` module** — it brings
  `tokio`/`tokio-util`/`async-trait` (the async sync loop) and `lmdb`/`lmdb-sys`
  (the freezer), neither of which may enter the async-free pure core. The pure
  `Chain` stays inside `zaino-nfs`; `zaino-core` is untouched. (Bonus: his
  `blake2` BLAKE2b checksums partially pre-answer Q3.)

### Open

- **Q3 — FS integrity + versioning.** Hahn's raw-block FS drops the current
  `FinalisedState` integrity/tamper model (record integrity, chain continuity,
  Merkle roots, spend/address indexes, FlyClient support) and has no
  migration story. Candidate vehicle: the primary/shadow routing in PR #1347.
  The typed-index layer restores *some* integrity; the block-FS still needs its
  own answer.

## Non-goals (not decided here)

- Mempool — a separate component that consumes the spine's tip signal.
- Wire/serving projections (compact/proto/verbose) — outer adapters over the port.

## Sources

- Hahn `zaino-store`: `DESIGN.md`, `docs/block-store-rationale.md`, `docs/BlockStore.lean` (PR #1378, branch `store`).
- PR #1378 review (K. Nuttycombe, idky137): snapshot coherence, side-branch retention, FS integrity, versioned-DB routing.
- Legacy NFS behavioural survey (this design thread): intent-vs-structure map of `NodeBackedChainIndex`.
- ADR-0003 (PR #1414): unconditional cross-request snapshot pinning — the requirement Q1 enforces.
- `docs/sync-engine/*`: the FS typed-index engine.
