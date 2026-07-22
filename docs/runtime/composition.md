# Runtime Composition — FS Index Engine ⊕ NFS Reorg Window

**Status:** design direction. Q1, Q2, Q4, Q5 resolved; Q3 open. This supersedes
the earlier "block-store spine ⊕ typed-index layer" framing — see **The key
insight**.

## Problem

The runtime (subdomain **C** — the orchestrator implementing
`zaino-service::IndexerService`) must handle the chain across **two operational
states**:

- **Catch-up (bulk, on boot):** fetch the whole finalized history and build the
  store + indexes. Throughput-bound, one-time, not fully operational
  (passthrough covers the gap).
- **Tip-follow (steady):** ingest one newly-mined block at a time, reorg-aware.
  Latency-bound, forever.

Plus a **snapshot** for coherent cross-request reads spanning both. Legacy
`NodeBackedChainIndex` does all this unsoundly (three validator pollers, a
fragile single-writer, reorg-unsafe `height→hash` lookups, an undetected
chain-shortening bug). Two efforts each cover part:

- **`zaino-sync`** — a parallel batch/DAG **index engine**. Great at bulk
  indexing; models no NFS/reorg.
- **Hahn's `zaino-store`** (PR #1378) — a two-tier block store with a
  **Lean-verified reorg algorithm** (`find_trim_index`); block-only, no typed
  indexes.

## The key insight

**The finalized block store is not a separate thing — it's one index in the
sync engine.** A compact block is just `height → CompactBlock`; stored as an
index it gives 1-read block serving inside the same backend as the aux indexes.
The current 8-way body decomposition (headers/txids/sapling/orchard/transparent
as separate indexes) is *coincidental* — nothing queries the pieces
individually (verified against the serving code) — so collapse it into one
compact-block index.

That dissolves the "block store vs index engine" split for the finalized state:
**the sync engine *is* the FS store.** And it collapses Hahn's role: with no
separate finalized block store needed, **Hahn contributes only the reorg-prone
window** — `Chain` + `find_trim_index` + snapshot. No `Freezer`, no forward-fill,
no `sync_step`.

## Decision

Two engines, split by **operational state** and by **finalized vs reorg-prone**,
meeting at the freeze horizon.

```
                     incoming reads (zaino-service::IndexerService)
                                       │
  ┌─────────────────────────────────────┼─────────────────────────────────────┐
  │ Snapshot = { pinned NFS Chain (+ side branches), FS watermark }             │
  │   recent (> watermark)   → pinned NFS Chain (reorg-bound, actively pinned)  │
  │   finalized (≤ watermark)→ FS index reads (append-only, passively coherent) │
  └─────────────────────────────────────┼─────────────────────────────────────┘
                                         │
  FS — sync engine (one backend)         │  NFS — reorg window (adopt Hahn)
  ──────────────────────────────         │  ────────────────────────────────
  compact_block : height → CompactBlock  │  Chain: in-mem ~101 blocks (im::Vector)
    (= the block store; pre_index +      │  find_trim_index: reorg (Lean-verified)
     tree_sizes fold, reassembled)       │  snapshot: pins the Chain
  hash_to_height, txid_location,         │  side-branch set (Q2)
  transparent_spends, address-history    │  aux queries here re-derive from Chain
  built: BULK parallel on boot,          │  driven: TIP-FOLLOW loop (1 block/iter)
         per-block at freeze             │
                                         │
       catch-up (bulk) ─────────── freeze horizon ─────────── tip-follow (steady)
```

### FS — the sync engine owns the finalized state (blocks + indexes)

One backend, uniform indexes:
- **`compact_block`** — `height → CompactBlock`; *this is the finalized block
  store*. 1-read serving. (Realized as `pre_index_compact_block` (L-scope) + a
  `tree_sizes` (S-scope) fold, reassembled at serve — see Q4.)
- **aux reverse indexes** — `hash_to_height`, `txid_location`,
  `transparent_spends`, address-history: the lookups a block store can't answer.
- The 8-way body decomposition is **dropped**.

Built by the sync engine's **parallel bulk pipeline on boot** (start-height →
`tip−D`), extended **per-block at freeze** in steady state. Only ever indexes
immutable (finalized) blocks — so it needs **no reorg machinery**.

### NFS — adopt Hahn's reorg window, and only that

The in-memory reorg-prone window:
- **`Chain`** (`im::Vector`, ~101 blocks) + a **side-branch set** (Q2) for
  fork-serving.
- **`find_trim_index`** — the Lean-verified reorg/fork-find (adopted
  near-verbatim).
- **snapshot** — pin an `Arc<Chain>` (Q1).
- Driven by a **tip-follow loop** (light: one block at a time).
- NFS aux queries **re-derive** from the `Chain` (cheap over ~101 blocks) — no
  persistent NFS index.

### The boundary

The **freeze horizon** (`tip − D`). At boot the bulk build fills FS to `tip−D`,
then the NFS `Chain` slow-syncs the last `D`. In steady state a block crossing
the horizon **freezes**: the tip-follower hands the already-fetched block to the
sync engine's per-block extract (block + aux → FS). **No redundant fetch** —
bulk covers `[start, tip−D]`, the `Chain` covers `[tip−D, tip]` (disjoint);
freeze reuses the fetched block.

## Ingestion & data flow

- **Boot / catch-up:** the sync engine's parallel bulk pipeline builds FS
  (compact-block index + aux) from the start height to `tip−D`. Passthrough
  serves un-built ranges; **serviceability advertises the synced height** (partial
  service during catch-up). Then the NFS `Chain` slow-syncs `tip−D → tip`.
- **Tip-follow (steady):** the tip-follow loop extends/reorgs the `Chain` (via
  `find_trim_index`) one block at a time. On freeze, the graduated block is
  extracted into FS. Reorgs only ever touch the `Chain`.
- **Single ingestor per range:** the bulk and tip paths are disjoint; freeze
  reuses the fetched block. No two components fetch the same range.

## The Snapshot

`Snapshot = { pinned Arc<Chain>, pinned Arc<side-branches>, FS watermark }`.
Recent reads (> watermark) hit the pinned `Chain`; finalized reads (≤ watermark)
hit the FS indexes (append-only, immutable — passively coherent, no versioning).
**FS-passive / NFS-pinned**, matching ADR-0003. First-class, client-held, held
until dropped (Q1).

## Serving

- **compact block by height** → FS `compact_block` index (finalized) or the NFS
  `Chain` (recent).
- **aux lookups** (hash→height, txid→loc, spend-status, address) → FS aux
  indexes (finalized) or re-derive from the `Chain` (recent).
- **fork queries** (getchaintips, orphaned-fork tx-status, fork-point) → the NFS
  side-branch set (Q2).
- **full `Block` / raw-tx / proofs** → **validator passthrough**, not stored
  (legacy `get_fullblock_bytes_from_node`; Q4). A full-block cache is a future
  optimization only if whole-chain full-block streaming (zallet scan) is hot.

## What we adopt vs keep

- **Adopt from Hahn (`zaino-store`) — narrow:** `Chain` (`im::Vector`),
  `find_trim_index` (Lean-verified reorg), the snapshot pin. **Not** the
  `Freezer`, forward-fill, or `sync_step` — superseded by the sync engine's bulk
  build + FS index backend.
- **Keep the sync engine (`zaino-sync`/`zaino-indexes`) whole**, change the index
  set: **add** `compact_block` (pre-index + `tree_sizes`), **keep** the aux
  reverse indexes, **drop** the 8-way body split.
- **Placement — a thin runtime + three delegated component crates**, each hiding
  its infra behind domain semantics:
  - **`zaino-fs`** — finalised state: elevates `zaino-sync`/`zaino-indexes` into
    finalised-state semantics (serve compact blocks + aux, bulk-build, freeze);
    hides the engine internally.
  - **`zaino-nfs`** — reorg window: `Chain` + `find_trim_index` + snapshot +
    side-branch set + tip-follow loop (`im`; **no LMDB**).
  - **`zaino-mempool`** — tip-tagged mempool (deferred).
  - **`zaino-runtime`** orchestrates the three and implements `IndexerService`;
    **`zaino-core` stays pure**.

## Design decisions

Resolved (verified against `zaino-store` code + legacy serving code):

- **Q1 — First-class snapshot.** Add a client-held `snapshot()` over the pinned
  `Arc<Chain>`; `im::Vector` supports it trivially (contra Hahn's rationale §3,
  per reviewers/ADR-0003).
- **Q2 — Side branches.** Retain a companion side-branch set on the NFS window
  for fork-serving.
- **Q4 — Finalized store = the sync engine's compact-block index**
  (`pre_index_compact_block` L-scope + `tree_sizes` S-scope, reassembled at
  serve; 2 reads) **+ aux reverse indexes**. Body decomposition dropped, **no
  separate Freezer**. (Full-`CompactBlock` 1-read serving needs X-scope deps,
  not yet built — take the 2-read for now.)
- **Q5 — Adopt only Hahn's NFS window** (`Chain` + `find_trim_index` + snapshot);
  the FS/bulk path is the sync engine. `sync_step`/`Freezer`/forward-fill not
  adopted.

Open:

- **Q3 — FS integrity + versioning.** The sync engine's FS has more structure
  than Hahn's raw blocks (typed indexes), but still needs the integrity/tamper
  model + migration story (candidate: PR #1347 primary/shadow routing).

## Non-goals (not decided here)

- Mempool — a separate component consuming the tip signal.
- Wire/serving projections (compact/proto/verbose) — outer adapters over the port.
- The `zaino-nfs` ↔ `zaino-sync` ↔ `zaino-runtime` seam traits — the next step
  (capabilities algebra).

## Sources

- Hahn `zaino-store` (PR #1378): `DESIGN.md`, `block-store-rationale.md`,
  `BlockStore.lean` — adopted narrowly (Chain + find_trim_index + snapshot).
- PR #1378 review (Nuttycombe, idky137): snapshot coherence, side-branch
  retention, FS integrity.
- Legacy NFS survey + block-type survey (this thread): intent + the
  CompactBlock/IndexedBlock/index-decomposition map.
- ADR-0003 (PR #1414): unconditional cross-request snapshot pinning.
- `docs/sync-engine/*`: the index engine.
