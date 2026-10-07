# zaino-nfs: one non-finalized state, one snapshot

Status: design, approved direction (2026-10-07). Supersedes the per-index RAM tier
(`zaino_persistence::Tiered`'s non-final half), the producer's above-final logic, `Step::Reorg` /
`Step::Finalized`, and the per-index serving gates. Builds on [verified-chain.md](verified-chain.md)
(the header chain decides best and final) and [persistence-engine.md](persistence-engine.md) (the
port).

## 1. The problem

The non-finalized chain lives in four places today: the header chain's best path, the producer's
held blocks, every index's RAM tier, and every index's serving gate. A reorg is replayed by the
producer and re-applied by every index. A gRPC request pins one index's view, so `GetLatestBlock`,
`GetBlockRange` and `GetTreeState` are only consistent because each index is gated separately.

## 2. The shape

```text
zaino-header-chain ── VerifiedChain (best, final) ─────────────┐
zaino-chainview ───── checked bodies (validators, peers) ──────┤
                                                               ▼
                                                   zaino-nfs: imbl graph by header hash
                                                     Node { parent, block, folded, layers }
                                                   ├─▶ Snapshot (one per tip change)
                                                   └─▶ final Folded → every writer (lockstep)
zaino-sync ── bulk: final blocks below the NFS root ─▶ writers fold + stage + commit
zaino-grpc ── snap = nfs.snapshot() ─▶ CompactBlockReader::at(&snap), TreeStateReader::at(&snap), …
```

- One pure function per index: `fold(parent: &Reader, block, inputs) -> Changes (+ outputs)`.
  The parent's state is *read*, never carried: compact-block reads the parent record's tree sizes,
  tree-state its frontier nodes, value-balance and transparent-address their maps. The reader is the
  same type gRPC serves from.
- Inter-index dependencies = fold order: value-balance folds first and returns the block's fees;
  compact-block's fold takes them.
- The NFS calls the folds at the tip. Writers call the same folds during bulk sync. One function,
  two callers.

## 3. Layers: the non-final data, as `imbl`

A `Layer` is one index's non-final data as of one node: per sequence table an
`imbl::Vector<Bytes>` of appended records, per map table an `imbl::OrdMap<key, value>` of inserts.
A node's layer = its parent's layer + its own `Changes`, built by structural sharing (O(log n) per
insert, O(1) to clone). Every node therefore holds the whole overlay as of itself.

`LayeredView<V> { durable: V, layer: Layer }` implements `SequenceRead` + `MapRead`: a position
past the durable length or a key in the layer answers from the layer, everything else from disk.
Each index's typed `Reader` wraps one `LayeredView`.

Consequences:
- A reorg = reading through the fork-side node's layer. No reverse fold, no replay, no refetch
  (the objection in [non-finalized-state.md](non-finalized-state.md) to rewinding was the reverse
  fold; persistent layers remove it).
- Switching back to a branch already folded costs nothing.
- Bulk staging uses the same `Layer`: a writer folds block `n + 1` against durable + its staged
  layer, then commits the batch.
- After finality the layer still holds what disk now holds (identical bytes, so reads stay correct).
  On each finality step the NFS rebases live layers onto the new root from the nodes' own
  `Changes` (bounded by the window; amortised, measured before tuning).

## 4. The graph (pure core + thin driver)

```rust
pub struct NfsCore {
    nodes: imbl::HashMap<BlockHash, Arc<Node>>,
    root: Option<BlockRef>,          // common durable tip of every enabled store
    best: Option<BlockHash>,         // verified best, as last seen
    served: Option<BlockRef>,        // deepest folded node on the verified best
    wants: Wants,                    // bodies to fetch (checked, any source)
}

pub struct Node { parent: BlockHash, at: BlockRef, block: Arc<Block>, folded: Arc<Folded> }

pub struct Folded {
    value_balance: Changes,
    fees: Arc<BlockFees>,
    compact_block: Option<Changes>,
    block_hash: Option<Changes>,
    tree_state: Option<Changes>,
    transparent_address: Option<Changes>,
    layers: Layers,                  // per index, = parent's + this node's Changes
}
```

Inputs (`step(Input, now) -> Vec<Output>`): a new `VerifiedChain`, a checked body, a fold result,
every store's commit acknowledgement, restart. Outputs: fetch wants, fold jobs (run on the blocking
pool, results come back as inputs), a new `Snapshot`, final `Folded` for the writers.

- Folds run when a node joins the verified best path (on demand), in parent order.
- Finality: the root's `Folded` goes to every writer; the node leaves the graph only after every
  enabled store acknowledges the commit (lockstep at the tip; one root for every index).
- Side branches stay within the header chain's bounds and are pruned with it.

## 5. The snapshot

```rust
pub struct Snapshot<V> {
    verified: Arc<VerifiedChain>,
    tip: BlockRef,                   // served tip: folded, on the verified best
    compact_block: Option<LayeredView<V>>,
    block_hash: Option<LayeredView<V>>,
    tree_state: Option<LayeredView<V>>,
    transparent_address: Option<LayeredView<V>>,
}
```

- Published through one `ArcSwap` + `watch` per tip change; `nfs.snapshot()` is one atomic load.
- Every request or stream pins one snapshot for its whole life: nodes stay alive through `Arc`s,
  disk through the pinned view. A commit or reorg mid-stream cannot move it.
- `GetLatestBlock` = `snap.tip`; every other RPC answers at heights `≤ snap.tip` from the same
  snapshot, so they agree by construction (R12 closed).
- The tip moves back on a reorg to the fork point until the new branch is folded, and on a pure
  rollback (Z#1577).
- Bulk sync: no NFS nodes; the snapshot is the durable views at the lowest durable tip of all
  enabled indexes, tip = that block.
- An index enabled later bulk-syncs alone; its slot in the snapshot is `None` (routes answer
  syncing) until its durable tip reaches the root, then the NFS starts folding it.

## 6. Writers

- Bulk: `fold` against durable + staged layer, stage, commit per `batch_mib` (one fsync).
  value-balance → compact-block fees keep flowing through `FeeSink` here (per-index tasks keep
  pipelining across indexes).
- Tip: receive the NFS's final `Folded` slice, append to the batch, commit, acknowledge.
- No `Reorg`, no `Finalized`, no RAM tier, no serving gate. `Step` = `Apply` (final) + `Shutdown`.

## 7. Invariants

| ID | Invariant | Where |
|---|---|---|
| N1 | every node's block = `VerifiedChain::hash_at` + merkle | `NfsCore::check`, fetch acceptance |
| N2 | node layer = parent layer + own `Changes` | `NfsCore::check` |
| N3 | a node leaves only after every enabled store's durable tip ≥ it | `NfsCore::check` |
| N4 | `snap.tip` on the verified best, folded, ≥ root | `NfsCore::check`, publish |
| N5 | final data never retracts; snapshot tips move back only above final | model |
| N6 | every index read through a snapshot = folding that index from genesis along best | model |

## 8. Tests

- `NfsCore` model: random verified-chain evolutions (extend, reorg at random depth, same-height
  replacement, retreat, finalize), bodies honest / wrong / slow / missing, commit acks delayed,
  restarts; stores on an in-memory engine (the port is generic). Oracle: fold every index from
  genesis along the best path; every snapshot answers like it at every height.
- Fire drills: one planted bug per `check()` assertion and precondition.
- Per index: fold golden tests + the existing reader/crash-state tests over `LayeredView`.
- `Layer`/`LayeredView` join the persistence conformance suite.

## 9. Rollout

0. Tiering cleanup lands (writers = block → `Changes`).
1. persistence: `Layer` + `LayeredView`; `Tiered`'s RAM tier removed, staging = a `Layer`.
2. indexes: `fold` + typed `Reader<V>` extracted; per-index RAM paths and gates removed.
3. `zaino-nfs`: core, model, fire drills, driver.
4. sync: producer = bulk below the NFS root + the checked fetch the NFS reuses; `Step` reduced.
5. grpc: routes over `Snapshot`; `Published`/`Served` gates removed (progress watches stay for
   status and metrics).
6. zainod: the approved boot and config.
7. docs, changesets, heavy runs, live suite (S1–S16 reorg group first).
