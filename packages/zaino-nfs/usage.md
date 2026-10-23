# `zaino-nfs` — usage

The tip overlay ([architecture §7](../../docs/architecture.md#tip-path-zaino-nfs)): every block
above the indexes' durable tips folded in RAM, one `NfsPublication` publish across every index (the index
half of `zaino_grpc_snapshot::GrpcSnapshot`). Finalized blocks reach the writers on the finalized path
(`zaino-sync`), never through here.

## Wiring: `Nfs`

```rust,ignore
use zaino_nfs::{ChainParams, Nfs, NfsError};

let params = ChainParams { network, schedule };                // activations() derived
let mut nfs = Nfs::new(header_sync.subscribe(), balancer.clone(), params, depth, lookahead);
let mut follower = FinalizedFollower::new(header_sync.subscribe(), balancer.clone(), lookahead);

// every enabled index, in IndexKind::ALL order: its handle to both, its finalized stream out
let mut fees = FeeWiring::new(compact_block_queue);     // None = compact-block off
for &kind in IndexKind::ALL {
    let store = engine.open(&path, &zaino_nfs::schema(kind, network), zaino_nfs::write_buffer(kind))?;
    let (handle, run) = zaino_nfs::writer(kind, store, &mut fees);
    let blocks = follower.subscribe(kind, handle.durable_view().tip(), queue_bytes);
    nfs.add(kind, handle);
    tasks.spawn(run(blocks));
}

let publisher = zaino_grpc_snapshot::Publisher::new(nfs.indexed(), view.subscriber(), depth);
tasks.spawn(nfs.run(cancel));                          // Err: Fold | ChainGone | IndexGone
```

- `Nfs::new`: the `HeaderChainSnapshot` watch, the traffic balancer (`zaino_sync::fetch` per wanted
  block, `Urgency::Tip`), the chain params, `depth` (sets the folding window:
  [architecture §7](../../docs/architecture.md#tip-path-zaino-nfs)), `lookahead` (bodies fetched
  or folding ahead of the next fold).
- `add(kind, handle)`: enables `kind` (panics on a kind twice). The handle's durable view is
  what publications and folds read.
- `run(cancel)`: cancel → `Ok`. `Fold` = an index's fold refused a verified block; `ChainGone` /
  `IndexGone(index)` = an input dropped.
- Every index by kind, one match per question (`indexes.rs`): `schema`, `write_buffer`, `fold`,
  `writer`. A new index = one arm in each + its `Views` reader.
- `FeeWiring`: utxo-set's fees → compact-block's writer, paired inside `writer`.

## Readers: `NfsPublication` → `IndexesAtBlock`

Routes never hold an `NfsPublication`: they load one `zaino_grpc_snapshot::GrpcSnapshot` per request and ask it
`served()?` (this crate's `IndexesAtBlock`).

```rust,ignore
let at = grpc_snapshot.served()?;                       // zaino-grpc-snapshot: one load, pinned
let tip = at.tip();                                     // GetLatestBlock: every read answers <= it
let blocks = at.views().compact_block().ok_or_else(disabled)?; // None = disabled, never syncing
let located = at.views().block_hash().map(|reader| reader.height_of(&hash));
```

- One `NfsPublication` = one served tip across every enabled index: each view = the durable view +
  the tip node's layer, so a commit or reorg mid-request moves nothing it reads.
- `IndexesAtBlock::answers_through(kind)` = the highest height `kind` answers: its durable tip when past a
  best-branch block (finalized data), else the block.
- `served().tip()` = the served tip ([architecture §7](../../docs/architecture.md#tip-path-zaino-nfs)).
  The publish is `None` (withdrawn) only with no folded best node and no root on the best chain.
- `at(hash)`: any folded node (best or side) or the root; `None` = below the root, never folded,
  or unknown.
- `durable()` = each enabled index's durable tip (`IndexKind::ALL` order), current as of the
  publish.
- `served_above_durable()` = the full blocks `served` extends above the root, served first (its own
  path only, side nodes never): what the mempool stream excludes.
- Feature `testing`: `NfsPublication::fixed(chain, tip, params, [(kind, view)])`;
  `NfsPublication::fixed_above(chain, root, &above_root_blocks, params, [(kind, view)])` (served =
  the last);
  `ChainParams::of(&mock_chain)`.

## Observability

| Signal | Meaning |
|---|---|
| `zaino_reorgs_total` (0 from boot) + WARN `Chain reorg detected` (`from`, `to`) | a published tip that left the best chain |
| INFO `Chain tip advanced` (`height`, `hash`, `age`, `finalized_tip`) | each published tip that is the verified best |

Fetch counters come from the finalized path (`zaino-sync`); `zaino_best_tip` and `zaino_fetch_height`
from `zaino-grpc-snapshot`.

## Core and invariants

```text
 HeaderChainSnapshot ┐                    ┌─▶ Fetch ───▶ zaino_sync::fetch(.., Tip) ─▶ Checked
 checked bodies ─────┤                    │
 fold results ───────┼─▶ NfsCore::step ───┼─▶ Fold ────▶ fold_block on rayon ─▶ Node in graph
 IndexHandle commits ┘   (pure: no I/O)   │
   (durable tips)                         └─▶ Publish ─▶ watch<Option<NfsPublication>> ─▶ zaino-grpc-snapshot
```

`NfsCore::step` is pure (no I/O): inputs = the chain, checked bodies, fold results, durable tips;
outputs = `Fetch`, `Abandon`, `Fold`, `Publish`. The driver is one task `select!`ing over them;
folds run on rayon. It reloads durable views only when it hands the core new durable tips, so
folds and publishes pair layers with exactly the tips the core knows.

Core, driver, folds and `Views` key every index one way, by `IndexKind` (`per_index.rs`):
`IndexSet` names indexes (the enabled set, a fold's covers), `PerIndex<T>` holds one value per
enabled index (`get` = `None` when disabled); both iterate in `IndexKind::ALL` (fold) order.

| ID | Invariant |
| --- | --- |
| N1 | every node's block = its own header + merkle root |
| N2 | nodes above the root, on a held parent |
| N3 | durable tips never move back |
| N4 | served tip = deepest folded best node, else the root |
| N5 | nothing folded, held or fetched outside the window |
| N6 | every index read through a publication = that index folded from genesis along best |
| N7 | `at(h)` is `Some` iff `h` is folded or the root, and reads = folding genesis..h along h's branch |
| N8 | every node on the best chain or a side branch the chain holds (`HeaderChainSnapshot::holds`) |

## Tests

- `nfs_core/model_tests.rs`: random chain evolutions, late and stale bodies and folds, writers committing
  after random delays, restarts with a wiped index, against a fold-from-genesis oracle.
- `nfs_core/fire_drills.rs`: one planted bug per `check()` assertion and precondition.
- `nfs/tests.rs`: `FinalizedFollower` + the six real writers + the NFS, end to end.

Heavy run required after changes: see [docs/testing.md](../../docs/testing.md#heavy-run).
