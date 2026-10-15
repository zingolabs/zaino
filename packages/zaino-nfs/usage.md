# `zaino-nfs` — usage

The tip overlay ([architecture §7](../../docs/architecture.md#tip-path-zaino-nfs)): every block
above the indexes' durable tips folded in RAM, one `Indexed` publish across every index (the index
half of `zaino_snapshot::GrpcSnapshot`). Final blocks reach the writers on the final path
(`zaino-sync`), never through here.

## Wiring: `Nfs`

```rust,ignore
use zaino_nfs::{ChainParams, Nfs, NfsError};

let params = ChainParams { network, schedule };                // activations() derived
let mut nfs = Nfs::new(header_sync.subscribe(), balancer.clone(), params, depth, lookahead);
let mut follower = FinalizedFollower::new(header_sync.subscribe(), balancer.clone(), lookahead);

// one per enabled index: its handle to both, its final stream out
let utxo_set_writer = UtxoSetIndexWriter::new(utxo_set_store);
let utxo_set = utxo_set_writer.handle();
let blocks = follower.subscribe(IndexKind::UtxoSet, utxo_set.view().tip(), queue_bytes);
nfs.add(IndexKind::UtxoSet, utxo_set);

let publisher = zaino_snapshot::Publisher::new(nfs.indexed(), view.subscriber(), depth);
tasks.spawn(nfs.run(cancel));                          // Err: Fold | ChainGone | IndexGone
```

- `Nfs::new`: the `VerifiedChain` watch, the traffic balancer (`zaino_sync::fetch` per wanted
  block, `Urgency::Tip`), the chain params, `depth` (the NFS folds only while the header just
  above the lowest durable tip is held and best is within `2 · depth` of it), `lookahead` (bodies
  fetched or folding ahead of the next fold).
- `add(kind, handle)`: enables `kind` (panics on a kind twice). The handle's durable view is
  what publications and folds read.
- `run(cancel)`: cancel → `Ok`. `Fold` = an index's fold refused a verified block; `ChainGone` /
  `IndexGone(index)` = an input dropped.

## Readers: `Indexed` → `At`

Routes never hold an `Indexed`: they load one `zaino_snapshot::GrpcSnapshot` per request and ask it
`served()?` (this crate's `At`).

```rust,ignore
let at = snap.served()?;                                // zaino-snapshot: one load, pinned
let tip = at.tip();                                     // GetLatestBlock: every read answers <= it
let blocks = at.views().compact_block().ok_or_else(disabled)?; // None = disabled, never syncing
let located = at.views().block_hash().map(|reader| reader.height_of(&hash));
```

- One `Indexed` = one served tip across every enabled index: each view = the durable view +
  the tip node's layer, so a commit or reorg mid-request moves nothing it reads.
- `At::answers_through(kind)` = the highest height `kind` answers: its durable tip when past a
  best-branch block (final data), else the block.
- `served().tip()` = the deepest folded block on the verified best, else the root (the lowest
  durable tip); a reorg moves it to the fork point at once. The publish is `None` (withdrawn) only
  with no folded best node and no root on the best chain.
- `at(hash)`: any folded node (best or side) or the root; `None` = below the root, never folded,
  or unknown. `At::branch()` = `Best` or `Side { from }`.
- `durable()` = each enabled index's durable tip, current as of the publish.
- `served_nonfinalized()` = the full blocks `served` extends above the root, served first (its own
  path only, side nodes never): what the mempool stream excludes.
- Feature `testing`: `Indexed::fixed(chain, tip, params, [(kind, view)])`;
  `Indexed::fixed_above(chain, root, &nonfinalized_blocks, params, [(kind, view)])` (served =
  the last);
  `ChainParams::of(&mock_chain)`.

## Observability

| Signal | Meaning |
|---|---|
| `zaino_reorgs_total` (0 from boot) + WARN `Chain reorg detected` (`from`, `to`) | a published tip that left the best chain |
| INFO `Chain tip advanced` (`height`, `hash`, `age`, `finalized`) | each published tip that is the verified best |

Fetch counters come from the final path (`zaino-sync`); `zaino_best_tip` and `zaino_fetch_height`
from `zaino-snapshot`.

## Core and invariants

```text
 VerifiedChain ──────┐                    ┌─▶ Fetch ───▶ zaino_sync::fetch(.., Tip) ─▶ Checked
 checked bodies ─────┤                    │
 fold results ───────┼─▶ NfsCore::step ───┼─▶ Fold ────▶ fold_block on rayon ─▶ Node in graph
 IndexHandle commits ┘   (pure: no I/O)   │
   (durable tips)                         └─▶ Publish ─▶ watch<Option<Indexed>> ─▶ zaino-snapshot
```

`NfsCore::step` is pure (no I/O): inputs = the chain, checked bodies, fold results, durable tips;
outputs = `Fetch`, `Abandon`, `Fold`, `Publish`. The driver is one task `select!`ing over them;
folds run on rayon. It reloads durable views only when it hands the core new durable tips, so
folds and publishes pair layers with exactly the tips the core knows.

| ID | Invariant |
| --- | --- |
| N1 | every node's block = its own header + merkle root, on the best chain or a held side |
| N2 | nodes above the root, on a held parent |
| N3 | durable tips never move back |
| N4 | served tip = deepest folded best node, else the root |
| N5 | nothing folded, held or fetched outside the window |
| N6 | every index read through a publication = that index folded from genesis along best |

## Tests

- `core/model.rs`: random chain evolutions, late and stale bodies and folds, writers committing
  after random delays, restarts with a wiped index, against a fold-from-genesis oracle.
- `core/fire_drills.rs`: one planted bug per `check()` assertion and precondition.
- `tests.rs`: `FinalizedFollower` + the five real writers + the NFS, end to end.

Heavy run required after changes: see [docs/testing.md](../../docs/testing.md#heavy-run).
